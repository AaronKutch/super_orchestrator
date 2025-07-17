use std::{
    collections::{HashMap, VecDeque},
    io::IsTerminal,
    path::PathBuf,
    sync::Arc,
};

// reexport from bollard
pub use bollard::secret::DeviceMapping;
use bollard::secret::{EndpointIpamConfig, EndpointSettings};
use futures::future::join_all;
use stacked_errors::{Result, StackableErr};
use tokio::{io::AsyncWriteExt, sync::Mutex};

use crate::{
    api_docker::{
        docker_socket::get_or_init_default_docker_instance, port_bindings_to_bollard_args,
        ContainerNetwork, DockerStdin, ExtraAddContainerOptions, PortBind, SuperImage,
        WaitContainer,
    },
    next_terminal_color,
};

/// The arguments to the API's equivalent of `docker create`.
#[derive(Debug, Clone, Default)]
pub struct ContainerCreateOptions {
    pub name: String,
    pub user: Option<String>,
    pub cmd: Vec<String>,
    pub env_vars: Vec<String>,
    pub volumes: Vec<(String, String)>,
    pub cap_adds: Vec<String>,
    pub sysctls: HashMap<String, String>,
    pub priviledged: bool,
    pub port_bindings: Vec<PortBind>,
    pub devices: Vec<DeviceMapping>,
    /// Wether the network should wait for the container to shutdown during a
    /// [ContainerNetwork::wait_important] call
    pub important: bool,
    /// Set to `Some`, this overwrites the default set in [ContainerNetwork].
    /// When enabled, the stdout and stderr of the container is logged to
    /// stderr.
    pub log_outs: Option<bool>,
}

/// A struct for the metadata regarding a running container
pub struct ContainerRunner {
    pub image: SuperImage,
    pub container_opts: ContainerCreateOptions,
    pub network_opts: ExtraAddContainerOptions,
    pub should_be_started: bool,
    pub stdin: Option<DockerStdin>,
    pub std_record: Option<Arc<Mutex<VecDeque<u8>>>>,
    pub wait_container: Option<WaitContainer>,
    // TODO hack to tell the error compilation if a container failed
    pub had_error: bool,
    pub std_log: Option<PathBuf>,
    pub debug: bool,
}

// for omitting the `stdin`
impl std::fmt::Debug for ContainerRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            image,
            container_opts,
            network_opts,
            should_be_started,
            std_log,
            ..
        } = self;
        write!(
            f,
            r#"LiveContainer {{
    image: {image:?},
    container_opts: {container_opts:?},
    network_opts: {network_opts:?},
    should_be_started: {should_be_started:?},
    std_log: {std_log:?},
}}"#
        )
    }
}

impl ContainerRunner {
    #[tracing::instrument(skip_all,
        fields(
            network.name = %network_name,
            container.name = %self.container_opts.name,
        )
    )]
    pub async fn start_container(
        &mut self,
        network_name: String,
        log_by_default: bool,
        write_logs: bool,
    ) -> Result<()> {
        // start_container already called for this container
        if self.should_be_started {
            return Ok(());
        }
        self.should_be_started = true;

        let docker = get_or_init_default_docker_instance().await.stack()?;

        let (exposed_ports, port_bindings) =
            port_bindings_to_bollard_args(&self.container_opts.port_bindings);

        // [docker reference](https://docs.docker.com/reference/api/engine/version/
        // v1.48/#tag/Container/operation/ContainerCreate)

        // https://github.com/moby/moby/issues/2949
        let (volumes, volume_binds) = Some((
            self.container_opts
                .volumes
                .iter()
                .map(|(_, container)| (container.to_string(), Default::default()))
                .collect(),
            self.container_opts
                .volumes
                .iter()
                .map(|(host, container)| format!("{host}:{container}"))
                .collect(),
        ))
        .unzip();

        if self.debug {
            tracing::debug!("Creating container");
        }

        docker
            .create_container(
                Some(bollard::container::CreateContainerOptions {
                    name: self.container_opts.name.clone(),
                    ..Default::default()
                }),
                bollard::container::Config {
                    hostname: self.network_opts.hostname.clone(),
                    user: self.container_opts.user.clone(),
                    exposed_ports,
                    cmd: Some(self.container_opts.cmd.clone()),
                    image: Some(self.image.get_image_id().to_string()),
                    volumes,
                    env: Some(self.container_opts.env_vars.clone()),
                    mac_address: self.network_opts.mac_address.clone(),
                    host_config: Some(bollard::secret::HostConfig {
                        cap_add: Some(self.container_opts.cap_adds.clone()),
                        sysctls: Some(self.container_opts.sysctls.clone()),
                        port_bindings,
                        binds: volume_binds,
                        privileged: Some(self.container_opts.priviledged),
                        devices: Some(self.container_opts.devices.clone()),
                        // don't flood user's containers
                        auto_remove: Some(true),
                        ..Default::default()
                    }),
                    // allows testing features
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(true),
                    open_stdin: Some(true),
                    // setup network
                    networking_config: Some(bollard::container::NetworkingConfig {
                        endpoints_config: [(network_name, EndpointSettings {
                            ipam_config: Some(EndpointIpamConfig {
                                ipv4_address: self
                                    .network_opts
                                    .ipv4_addr
                                    .as_ref()
                                    .map(ToString::to_string),
                                ipv6_address: self
                                    .network_opts
                                    .ipv6_addr
                                    .as_ref()
                                    .map(ToString::to_string),
                                ..Default::default()
                            }),
                            ..Default::default()
                        })]
                        .into_iter()
                        .collect(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .inspect(|x| {
                if self.debug {
                    tracing::debug!(container.id = %x.id)
                }
            })
            .stack()?;

        if self.debug {
            tracing::debug!("Starting container");
        }

        // Note: it is extremely important that we call all the things we need to before
        // `start_container` is called. For instance, if `attach_container` takes too
        // long, it will miss part of the logs or even miss the duration of the
        // container entirely resulting in an error. `wait_container` could also miss
        // the container entirely, but we can call both before `start_container` is even
        // called.

        // `WaitContainerOptions` seems to do nothing
        let wait_container = docker.wait_container::<String>(&self.container_opts.name, None);

        let response = docker
            .attach_container(
                &self.container_opts.name,
                Some(bollard::container::AttachContainerOptions {
                    stdin: Some(true),
                    stdout: Some(true),
                    stderr: Some(true),
                    stream: Some(true),
                    logs: Some(true),
                    detach_keys: Some("ctrl-c"),
                }),
            )
            .await
            .stack()?;

        docker
            .start_container::<String>(&self.container_opts.name, None)
            .await
            .stack()?;

        self.stdin = Some(response.input);
        self.wait_container = Some(Box::pin(wait_container));
        let std_record = Arc::new(Mutex::new(VecDeque::new()));
        self.std_record = Some(std_record.clone());

        // log docker outputs variables
        let container_name = self.container_opts.name.clone();
        let mut output = response.output;
        let log_output = self.container_opts.log_outs.unwrap_or(log_by_default);
        let mut std_log = if let Some(log_file) = self
            .std_log
            .as_ref()
            .and_then(|output_dir| write_logs.then(|| output_dir.clone()))
        {
            tokio::fs::File::options()
                .write(true)
                .create(true)
                .truncate(true)
                .read(true)
                .open(log_file)
                .await
                .inspect_err(|err| tracing::warn!("When opening log file: {err}"))
                .ok()
        } else {
            None
        };

        // log docker outputs handler

        // TODO but we need the `std_record` for error compilation
        //if log_output || std_log.is_some() || std_record.is_enabled {
        tokio::spawn(async move {
            use bollard::container::LogOutput;
            use futures::stream::StreamExt;

            let (prefix_out, prefix_err) = if std::io::stderr().is_terminal() {
                let terminal_color = next_terminal_color();
                (
                    owo_colors::OwoColorize::color(
                        &format!("{container_name}  | "),
                        terminal_color,
                    )
                    .to_string(),
                    owo_colors::OwoColorize::color(
                        &format!("{container_name} E| "),
                        terminal_color,
                    )
                    .to_string(),
                )
            } else {
                Default::default()
            };

            let prefix_err_newline = "\n".to_string() + &prefix_err;
            let prefix_out_newline = "\n".to_string() + &prefix_out;

            while let Some(output) = output.next().await {
                match output.stack()? {
                    LogOutput::StdErr { message } => {
                        if log_output {
                            eprintln!(
                                "{prefix_err}{}",
                                &String::from_utf8_lossy(&message)
                                    .lines()
                                    .collect::<Vec<_>>()
                                    .join(&prefix_err_newline)
                            )
                        }
                        if let Some(ref mut log_file) = std_log {
                            log_file.write_all(&message).await.stack()?;
                        }
                        std_record.lock().await.extend(message.iter());
                    }
                    // not sure why but all output comes from LogOutput::Console
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        if log_output {
                            eprintln!(
                                "{prefix_out}{}",
                                &String::from_utf8_lossy(&message)
                                    .lines()
                                    .collect::<Vec<_>>()
                                    .join(&prefix_out_newline)
                            )
                        }
                        if let Some(ref mut log_file) = std_log {
                            log_file.write_all(&message).await.stack()?;
                        }
                        std_record.lock().await.extend(message.iter());
                    }
                    LogOutput::StdIn { message: _ } => (),
                }
            }

            Ok(()) as Result<_>
        });

        Ok(())
    }

    pub async fn stop_container(
        &mut self,
        options: Option<bollard::container::StopContainerOptions>,
    ) -> Result<()> {
        if !self.should_be_started {
            return Ok(());
        }

        let docker = get_or_init_default_docker_instance().await.stack()?;
        docker
            .stop_container(&self.container_opts.name, options)
            .await
            .stack()?;
        //only after confirming stopped
        self.should_be_started = false;
        Ok(())
    }
}

/// Tears down a docker network and all of its containers
#[tracing::instrument(skip_all,
    fields(
        network.name = %network_name,
    )
)]
pub async fn total_teardown(
    network_name: &String,
    known_container_names: impl IntoIterator<Item = String>,
) -> Result<()> {
    let docker = get_or_init_default_docker_instance().await.stack()?;

    let live_containers = docker
        .inspect_network::<String>(network_name, None)
        .await
        .stack()?
        .containers
        .map_or_else(Vec::new, |containers| containers.into_keys().collect());

    let futs = live_containers
        .into_iter()
        .chain(known_container_names)
        .map(|container_name| {
            let docker = docker.clone();

            Box::pin(async move {
                if let Ok(Some(container)) = ContainerNetwork::inspect_container(&container_name)
                    .await
                    .stack()
                {
                    if container.running.is_some_and(|x| x) {
                        docker
                            .stop_container(
                                &container_name,
                                Some(bollard::container::StopContainerOptions { t: 0 }),
                            )
                            .await
                            .inspect_err(|err| {
                                tracing::debug!("failed to shutdown container Err: {err}")
                            })
                            .stack()?;
                    }
                }

                Ok(()) as Result<_>
            })
        })
        .collect::<Vec<_>>();

    let mut errs = join_all(futs)
        .await
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();

    if let Err(err) = docker.remove_network(network_name).await.stack() {
        errs.push(err)
    };

    if let Some(last_err) = errs.pop() {
        Err(errs
            .into_iter()
            .fold(last_err, |last_err, err| last_err.chain_errors(err)))
    } else {
        Ok(())
    }
}
