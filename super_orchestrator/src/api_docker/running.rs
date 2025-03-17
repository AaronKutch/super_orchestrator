use std::{collections::HashMap, io::IsTerminal, path::PathBuf};

use bollard::secret::DeviceMapping;
use stacked_errors::{Result, StackableErr};
use tokio::io::AsyncWriteExt;

use crate::{
    api_docker::{
        docker_socket::get_or_init_default_docker_instance, port_bindings_to_bollard_args,
        DockerStdin, PortBind, SuperImage, SuperNetwork, SuperNetworkContainerOptions,
    },
    next_terminal_color,
};

/// Define `docker create` arguments with integration testing in mind. You can
/// construct a container using the [SuperNetwork] struct.
#[derive(Debug, Clone, Default)]
pub struct SuperContainerOptions {
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
    /// [SuperNetwork::wait_important] call
    pub important: bool,
    /// When defined, overwrites the default set in [SuperNetwork].
    ///
    /// When log_outs is activated the network will use `docker attach` to read
    /// stdout and stderr from container and log to stderr
    pub log_outs: Option<bool>,
}

pub struct LiveContainer {
    pub image: SuperImage,
    pub container_opts: SuperContainerOptions,
    pub network_opts: SuperNetworkContainerOptions,
    pub should_be_started: bool,
    pub stdin: Option<DockerStdin>,
    pub output_dir: Option<PathBuf>,
}

impl std::fmt::Debug for LiveContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            image,
            container_opts,
            network_opts,
            should_be_started,
            ..
        } = self;
        write!(
            f,
            r#"LiveContainer {{
    image: {image:?},
    container_opts: {container_opts:?},
    network_opts: {network_opts:?}
    should_be_started: {should_be_started:?},
}}"#
        )
    }
}

#[tracing::instrument(skip_all,
    fields(
        network.name = %network_name,
        container.name = %live_container.container_opts.name,
    )
)]
pub async fn start_container(
    live_container: &mut LiveContainer,
    network_name: String,
    log_by_default: bool,
    write_logs: bool,
) -> Result<()> {
    // start_container already called for this container
    if live_container.should_be_started {
        return Ok(())
    }
    live_container.should_be_started = true;

    let docker = get_or_init_default_docker_instance().await.stack()?;

    let (exposed_ports, port_bindings) =
        port_bindings_to_bollard_args(&live_container.container_opts.port_bindings);

    #[rustfmt::skip] /* because of comment size */
    // [docker reference](https://docs.docker.com/reference/api/engine/version/v1.48/#tag/Container/operation/ContainerCreate)
    // https://github.com/moby/moby/issues/2949
    let (volumes, volume_binds) = Some((
        live_container
            .container_opts
            .volumes
            .iter()
            .map(|(_, container)| (container.to_string(), Default::default()))
            .collect(),
        live_container
            .container_opts
            .volumes
            .iter()
            .map(|(host, container)| format!("{host}:{container}"))
            .collect(),
    ))
    .unzip();

    tracing::debug!("Creating container");

    docker
        .create_container(
            Some(bollard::container::CreateContainerOptions {
                name: live_container.container_opts.name.clone(),
                ..Default::default()
            }),
            bollard::container::Config {
                hostname: live_container.network_opts.hostname.clone(),
                user: live_container.container_opts.user.clone(),
                exposed_ports,
                cmd: Some(live_container.container_opts.cmd.clone()),
                image: Some(live_container.image.get_image_id().to_string()),
                volumes,
                env: Some(live_container.container_opts.env_vars.clone()),
                mac_address: live_container.network_opts.mac_address.clone(),
                host_config: Some(bollard::secret::HostConfig {
                    cap_add: Some(live_container.container_opts.cap_adds.clone()),
                    sysctls: Some(live_container.container_opts.sysctls.clone()),
                    port_bindings,
                    binds: volume_binds,
                    privileged: Some(live_container.container_opts.priviledged),
                    devices: Some(live_container.container_opts.devices.clone()),
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
                    endpoints_config: [(
                        network_name,
                        // TODO: Could add some configuration for the network here
                        Default::default(),
                    )]
                    .into_iter()
                    .collect(),
                }),
                ..Default::default()
            },
        )
        .await
        .inspect(|x| tracing::debug!(container.id = %x.id))
        .stack()?;

    tracing::debug!("Starting container");

    docker
        .start_container::<String>(&live_container.container_opts.name, None)
        .await
        .stack()?;

    tracing::debug!("Attaching to container");

    let response = docker
        .attach_container(
            &live_container.container_opts.name,
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

    live_container.stdin = Some(response.input);

    // log docker outputs variables
    let container_name = live_container.container_opts.name.clone();
    let mut output = response.output;
    let log_output = live_container
        .container_opts
        .log_outs
        .unwrap_or(log_by_default);
    let mut log_file = if let Some(mut log_file) = live_container
        .output_dir
        .as_ref()
        .and_then(|output_dir| write_logs.then(|| output_dir.clone()))
    {
        log_file.push("super_log");
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
    if log_output || log_file.is_some() {
        tokio::spawn(async move {
            use bollard::container::LogOutput;
            use futures::stream::StreamExt;

            let (prefix_out, prefix_err) = if std::io::stderr().is_terminal() {
                let terminal_color = next_terminal_color();
                (
                    owo_colors::OwoColorize::color(
                        &format!("{container_name}   | "),
                        terminal_color,
                    )
                    .to_string(),
                    owo_colors::OwoColorize::color(
                        &format!("{container_name}  E| "),
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
                        if let Some(ref mut log_file) = log_file {
                            log_file.write_all(&message).await.stack()?;
                        }
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
                        if let Some(ref mut log_file) = log_file {
                            log_file.write_all(&message).await.stack()?;
                        }
                    }
                    _ => {}
                }
            }

            Ok(()) as Result<_>
        });
    }

    Ok(())
}

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

    let mut futs = live_containers
        .into_iter()
        .chain(known_container_names)
        .map(|container_name| {
            let docker = docker.clone();

            Box::pin(async move {
                if let Ok(Some(container)) = SuperNetwork::inspect_container(&container_name)
                    .await
                    .stack()
                {
                    if container.running.is_some_and(|x| x) {
                        docker
                            .stop_container(
                                &container_name,
                                Some(bollard::container::StopContainerOptions { t: 5 }),
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

    let mut errs = vec![];

    while !futs.is_empty() {
        let (res, _, rest) = futures::future::select_all(futs).await;
        if let Err(err) = res {
            errs.push(err)
        };
        futs = rest;
    }

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
