use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

pub use bollard::{
    container::{AttachContainerResults, LogOutput},
    errors::Error as BollardError,
    image::BuildImageOptions,
    secret::{ContainerState, DeviceMapping, Ipam, IpamConfig},
};
use stacked_errors::{Result, StackableErr};
use tracing::{Instrument, Level};

use crate::api_docker::{
    docker_socket::get_or_init_default_docker_instance, start_container, total_teardown,
    DockerStdin, LiveContainer, SuperContainerOptions, SuperDockerfile, SuperImage,
    SUPER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME,
};

/// Manages as set containers in a controlled environment.
/// Useful for creating integration tests and examples.
///
/// This module uses [SuperDockerfile]s to create containers for testing and
/// adds a simple way to declare docker networks, manage conatainers in the
/// networks and can compile the outputs for effective testing.
#[derive(Debug)]
pub struct SuperNetwork {
    // might be good for debug
    #[allow(dead_code)]
    network_id: String,
    opts: SuperCreateNetworkOptions,
    containers: HashMap<String, LiveContainer>,
}

#[derive(Debug)]
pub enum AddContainerOptions {
    /// Use an already specified image to create the container
    Container {
        image: SuperImage,
    },
    DockerFile {
        docker_file: SuperDockerfile,
    },
    BollardArgs {
        bollard_args: (BuildImageOptions<String>, Vec<u8>),
    },
}

#[derive(Debug, Clone, Default)]
pub struct SuperCreateNetworkOptions {
    pub name: String,
    pub driver: Option<String>,
    pub enable_ipv6: bool,
    pub options: HashMap<String, String>,
    pub labels: HashMap<String, String>,
    pub ipam: Ipam,
    /// If set the network will shutdown and start a new network if there's a
    /// name collision.
    pub overwrite_existing: bool,
    /// Configure an output directory for logging/assertions.
    pub output_dir_config: Option<OutputDirConfig>,
    /// If true, [SuperContainerOptions] with `log_outs: None` will use this
    /// value as default
    pub log_by_default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct OutputDirConfig {
    /// Directory for dealing with outputs.
    ///
    /// Set value as a temporary directory or an
    /// ignored directory in a repository. This won't mount the output dir,
    /// it'll create other directories and mount them (by binding).
    ///
    /// This also adds env var SUPER_NETWORK_OUTPUT_DIR to the container. Add
    /// outputs using the env var to ensure compatibility.
    pub output_dir: String,
    /// Write all captured output to a log file
    pub save_logs: bool,
}

/// If any field is None, it'll be equivalent to passing no argument to `docker
/// create` command.
#[derive(Debug, Clone, Default)]
pub struct SuperNetworkContainerOptions {
    pub hostname: Option<String>,
    pub mac_address: Option<String>,
}

impl SuperNetwork {
    pub fn teardown_on_ctrlc<'a>(cns: impl IntoIterator<Item = &'a SuperNetwork>) {
        let futs = cns
            .into_iter()
            .map(|cn| {
                let cn_name = cn.opts.name.clone();

                |span: tracing::Span| {
                    Box::pin(async move {
                        let _enter = span.enter();

                        let _ = total_teardown(&cn_name, [])
                            .await
                            .stack()
                            .inspect_err(|err| tracing::error!("{err}"))
                            .in_current_span();
                    })
                }
            })
            .collect::<Vec<_>>();
        tokio::task::spawn(async move {
            let span = tracing::span!(Level::INFO, "ctrlc handler");
            let _enter = span.enter();

            tracing::info!("ctrlc will teardown all networks");
            if tokio::signal::ctrl_c()
                .await
                .stack_err("Failed to wait for ctrlc")
                .is_err()
            {
                std::process::exit(1);
            }
            tracing::info!("ctrlc detected, TEARING DOWN NETWORKS");
            // also log to stdout because it's immediate
            eprintln!("ctrlc detected, TEARING DOWN NETWORKS");

            futures::future::join_all(futs.into_iter().map(|fut| fut(span.clone()))).await;

            std::process::exit(1);
        });
    }

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
                        SuperDockerfile::build_with_bollard_defaults(bollard_args.0, bollard_args.1)
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
                SuperDockerfile::build_with_bollard_defaults(bollard_args.0, bollard_args.1)
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

    /// Wait for all listed containers to be health.
    ///
    /// If a container doesn't have a healthcheck it's automatically considered
    /// healthy.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
        )
    )]
    pub async fn wait_healthy(
        &self,
        container_names: impl IntoIterator<Item = impl ToString>,
    ) -> Result<()> {
        let futs =
            container_names
                .into_iter()
                .map(|container_name| {
                    let container_name = container_name.to_string();

                    || {
                        Box::pin(async move {
                            Self::inspect_container(&container_name)
                                .await
                                .stack()
                                .map(|res| {
                                    res.is_some_and(|status| {
                                        if let Some(health) = status.health {
                                            health.status.is_some_and(|health_status| {
                                                match health_status {
                                bollard::secret::HealthStatusEnum::STARTING => {
                                    tracing::debug!("{container_name} starting");
                                    false
                                },
                                bollard::secret::HealthStatusEnum::HEALTHY => {
                                    tracing::info!("{container_name} healthy!");
                                    true
                                },
                                bollard::secret::HealthStatusEnum::UNHEALTHY => {
                                    tracing::debug!("{container_name} unhealthy");
                                    false
                                }
                                bollard::secret::HealthStatusEnum::EMPTY |
                                bollard::secret::HealthStatusEnum::NONE => {
                                    tracing::warn!("No healthcheck for container {container_name}");
                                    true
                                }
                            }
                                            })
                                        } else {
                                            tracing::warn!(
                                                "No healthcheck for container {container_name}"
                                            );
                                            true
                                        }
                                    })
                                })
                        })
                    }
                })
                .collect::<Vec<_>>();

        let mut finished = false;
        while !finished {
            let mut futs = futs.clone().into_iter().map(|x| x()).collect::<Vec<_>>();
            finished = true;
            while !futs.is_empty() {
                let (res, _, rest) = futures::future::select_all(futs).await;
                finished |= res.stack()?;
                futs = rest;
            }
        }

        Ok(())
    }
}
