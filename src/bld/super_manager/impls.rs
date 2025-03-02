use std::{collections::HashSet, io::IsTerminal, path::PathBuf, str::FromStr, sync::Arc};

use stacked_errors::{Result, StackableErr};
use tokio::io::AsyncWriteExt;

use super::*;
use crate::{bld::docker_socket::get_or_init_default_docker_instance, next_terminal_color};

impl PortBind {
    /// Results in option like <port>:<port>
    pub fn new(port: u16) -> Self {
        Self {
            container_port: port,
            host_port: Some(port),
            host_ip: None,
        }
    }

    /// Results in option like <host_port>:<container_port>
    pub fn with_host_port(mut self, port: u16) -> Self {
        self.host_port = Some(port);
        self
    }

    /// Results in option like <ip>:<host_port>:<container_port>
    pub fn with_host_ip(mut self, ip: IpAddr) -> Self {
        self.host_ip = Some(ip);
        self
    }
}

impl From<u16> for PortBind {
    fn from(port: u16) -> Self {
        Self::new(port)
    }
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

impl SuperNetwork {
    /// opts is a passthrough argument to [bollard::Docker::create_network]
    ///
    /// overwrite_existing: In case network name collides, should I teardown the
    /// network?
    ///
    /// Uses default docker instance from
    /// [bollard::Docker::connect_with_defaults]
    #[tracing::instrument(skip_all,
        fields(
            network.name = %opts.name,
        )
    )]
    pub async fn create(opts: SuperCreateNetworkOptions) -> Result<Self> {
        let docker = get_or_init_default_docker_instance().await.stack()?;

        if let Some(network_name) = docker
            .list_networks::<String>(None)
            .await
            .stack()?
            .into_iter()
            .find_map(|network| {
                (network.name.as_ref() == Some(&opts.name)).then_some(network.name.unwrap())
            })
        {
            if opts.overwrite_existing {
                tracing::debug!("Tearing down name match for {network_name} connection");
                total_teardown(&network_name, std::iter::empty())
                    .await
                    .stack()?;
            } else {
                return Err("network {network_name} already exists").stack();
            }
        }

        let response = docker
            .create_network(bollard::network::CreateNetworkOptions {
                name: opts.name.clone(),
                driver: opts.driver.clone().unwrap_or_else(|| "bridge".to_string()),
                enable_ipv6: opts.enable_ipv6,
                options: opts.options.clone(),
                labels: opts.labels.clone(),
                ipam: opts.ipam.clone(),
                ..Default::default()
            })
            .await
            .stack()?;

        tracing::info!(
            "network name: {}\n network id: {}\n message: {}",
            opts.name,
            response.id,
            response.warning
        );

        Ok(Self {
            network_id: response.id,
            opts,
            containers: Default::default(),
        })
    }

    /// This DOESN'T call `docker create` or `docker start`. It may call `docker
    /// build` if necessary. This function will return error if the
    /// container is already registered in the network.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
            container.name = %container.name,
        )
    )]
    pub async fn add_container(
        &mut self,
        mut add_opts: AddContainerOptions,
        network_opts: SuperNetworkContainerOptions,
        mut container: SuperContainerOptions,
    ) -> Result<()> {
        if self.containers.contains_key(&container.name) {
            return Err("Name is already registered").stack();
        }

        if container.name.is_empty() {
            return Err("Name for container can't be empty").stack();
        }

        let output_dir = if let Some(ref output_config) = self.opts.output_dir_config {
            add_opts = AddContainerOptions::DockerFile {
                docker_file: match add_opts {
                    AddContainerOptions::Container { image } => image.to_docker_file(),
                    AddContainerOptions::DockerFile { docker_file } => docker_file,
                    AddContainerOptions::BollardArgs { bollard_args } => {
                        SuperDockerFile::build_with_bollard_defaults(bollard_args.0, bollard_args.1)
                            .await
                            .stack()?
                            .0
                            .to_docker_file()
                    }
                }
                .appending_dockerfile_instructions(["RUN mkdir /super_out"]),
            };

            let mut output_dir = PathBuf::from_str(&output_config.output_dir).stack()?;
            output_dir.push(&container.name);
            let output_dir_str = output_dir.to_str().unwrap();

            if let Err(err) = tokio::fs::create_dir(&output_dir).await {
                match err.kind() {
                    std::io::ErrorKind::AlreadyExists => {
                        if output_dir_str == "/" {
                            return Err(format!(
                                "Trying to create output_dir at {output_dir_str} for {}",
                                container.name
                            ))
                            .stack();
                        }

                        tracing::warn!(
                            "Output directory for container {} already exists.",
                            container.name
                        );
                    }
                    _ => {
                        return Err(format!(
                            "Problems creating output_dir ({output_dir_str}) for container {}",
                            container.name
                        ))
                        .stack()
                    }
                }
            }

            container
                .volumes
                .push((output_dir_str.to_string(), "/super_out".to_string()));
            container.env_vars.push(format!(
                "{SUPER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME}=/super_out"
            ));

            Some(output_dir)
        } else {
            None
        };

        let image = match add_opts {
            AddContainerOptions::Container { image } => image,
            AddContainerOptions::DockerFile { docker_file } => {
                docker_file.build_image().await.stack()?.0
            }
            AddContainerOptions::BollardArgs { bollard_args } => {
                SuperDockerFile::build_with_bollard_defaults(bollard_args.0, bollard_args.1)
                    .await
                    .stack()?
                    .0
            }
        };

        self.containers
            .insert(container.name.clone(), LiveContainer {
                should_be_started: false,
                image,
                container_opts: container,
                network_opts,
                stdin: None,
                output_dir,
            });

        Ok(())
    }

    /// Call `docker create` and `docker start` on container using its options.
    /// This will mark the container with flag should_be_started to true. If
    /// this flag is set for the container the docker commands won't be
    /// executed.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
            container.name = %container_name,
        )
    )]
    pub async fn start_container(&mut self, container_name: &str) -> Result<()> {
        let Some(live_container) = self.containers.get_mut(container_name) else {
            return Err(format!("Container with name {container_name} not found")).stack();
        };

        start_container(
            live_container,
            self.opts.name.clone(),
            self.opts.log_by_default,
            self.opts
                .output_dir_config
                .as_ref()
                .is_some_and(|config| config.save_logs),
        )
        .await
        .stack()
    }

    /// Calls [SuperNetwork::start_container] for all registered containers.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
        )
    )]
    pub async fn start_all(&mut self) -> Result<()> {
        let network_name = self.opts.name.clone();

        let mut futs = self
            .containers
            .values_mut()
            .map(|live_container| {
                Box::pin(start_container(
                    live_container,
                    network_name.clone(),
                    self.opts.log_by_default,
                    self.opts
                        .output_dir_config
                        .as_ref()
                        .is_some_and(|config| config.save_logs),
                ))
            })
            .collect::<Vec<_>>();

        while !futs.is_empty() {
            let (res, _, rest) = futures::future::select_all(futs).await;
            res.stack()?;
            futs = rest;
        }

        Ok(())
    }

    #[tracing::instrument(skip_all,
        fields(
            container.name = %container_name,
        )
    )]
    pub async fn inspect_container(container_name: &str) -> Result<Option<ContainerState>> {
        let docker = get_or_init_default_docker_instance().await.stack()?;

        Ok(docker
            .inspect_container(container_name, None)
            .await
            .ok()
            .and_then(|res| res.state))
    }

    /// Waits for all containers marked as important to shutdown
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
        )
    )]
    pub async fn wait_important(&self) -> Result<()> {
        let already_down = Arc::new(tokio::sync::RwLock::new(HashSet::new()));
        let importants = self
            .containers
            .iter()
            .filter(|(_, container)| container.container_opts.important)
            .collect::<Vec<_>>();

        tracing::debug!("total important: {}", importants.len());

        let build_futs = || {
            importants
                .iter()
                .map(|(container_name, _)| {
                    let already_down = already_down.clone();
                    Box::pin(async move {
                        if already_down.read().await.contains(*container_name) {
                            return Ok(()) as Result<_>;
                        }

                        let status = Self::inspect_container(container_name).await.stack()?;

                        if status
                            .and_then(|status| status.running.map(|x| !x))
                            .unwrap_or(true)
                        {
                            already_down
                                .write()
                                .await
                                .insert(container_name.to_string());
                        }

                        Ok(())
                    })
                })
                .collect::<Vec<_>>()
        };

        while importants.len() != already_down.read().await.len() {
            let mut futs = build_futs();

            while !futs.is_empty() {
                let (res, _, rest) = futures::future::select_all(futs).await;
                res.stack()?;
                futs = rest;
            }

            tracing::debug!(
                "total importants shutdown: {}/{}",
                already_down.read().await.len(),
                importants.len()
            );

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    /// When the container is started, [SuperNetwork] automatically attaches to
    /// its stdin and outputs using [bollard::Docker::attach_container].
    ///
    /// This retrieves the stdin resulting from the attachment
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
            container.name = %container_name,
        )
    )]
    pub async fn get_stdin(&mut self, container_name: &str) -> Option<&mut DockerStdin> {
        self.containers
            .get_mut(container_name)
            .and_then(|container| container.stdin.as_mut())
    }

    /// Try stopping all containers and delete network.
    ///
    /// It'll always try to complete the full teardown and aggregate the errors.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
        )
    )]
    pub async fn teardown(self) -> Result<()> {
        total_teardown(&self.opts.name, self.containers.into_keys())
            .await
            .stack()
    }
}

#[tracing::instrument(skip_all,
    fields(
        network.name = %network_name,
    )
)]
async fn total_teardown(
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

#[tracing::instrument(skip_all,
    fields(
        network.name = %network_name,
        container.name = %live_container.container_opts.name,
    )
)]
async fn start_container(
    live_container: &mut LiveContainer,
    network_name: String,
    log_by_default: bool,
    write_logs: bool,
) -> Result<()> {
    // start_container already called for this container
    if live_container.should_be_started {
        return Ok(())
    }

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

            while let Some(output) = output.next().await {
                match output.stack()? {
                    LogOutput::StdErr { message } => {
                        if log_output {
                            eprintln!(
                                "{prefix_err}{}",
                                &String::from_utf8_lossy(&message)
                                    .split('\n')
                                    .collect::<Vec<_>>()
                                    .join(&prefix_err)
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
                                    .split('\n')
                                    .collect::<Vec<_>>()
                                    .join(&prefix_err)
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

#[allow(clippy::type_complexity)] /* internal only */
fn port_bindings_to_bollard_args(
    pbs: &[PortBind],
) -> (
    Option<HashMap<String, HashMap<(), ()>>>,
    Option<HashMap<String, Option<Vec<bollard::secret::PortBinding>>>>,
) {
    Some(
        pbs.iter()
            .map(|pb| {
                (
                    (pb.container_port.to_string(), HashMap::new()),
                    (
                        pb.container_port.to_string(),
                        Some(vec![bollard::secret::PortBinding {
                            host_port: pb
                                .host_port
                                .or(Some(pb.container_port))
                                .as_ref()
                                .map(ToString::to_string),
                            host_ip: pb.host_ip.as_ref().map(ToString::to_string),
                        }]),
                    ),
                )
            })
            .unzip(),
    )
    .unzip()
}
