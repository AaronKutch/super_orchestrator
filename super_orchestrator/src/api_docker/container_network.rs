use std::{
    collections::HashMap,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

// reexports from bollard. `IpamConfig` is reexported because it is part of `Ipam`
pub use bollard::secret::{ContainerState, Ipam, IpamConfig};
use futures::StreamExt;
use stacked_errors::{Error, Result, StackableErr};
use tokio::{select, time::sleep};
use tracing::{Instrument, Level};

use crate::api_docker::{
    docker_socket::get_or_init_default_docker_instance, total_teardown, ContainerCreateOptions,
    ContainerRunner, DockerStdin, SuperDockerfile, SuperImage,
    CONTAINER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME,
};

/// Manages a set of containers in a controlled environment.
/// Useful for creating integration tests and examples.
///
/// This module uses [SuperDockerfile]s to create containers for testing and
/// adds a simple way to declare docker networks, manage conatainers in the
/// networks and can compile the outputs for effective testing.
#[derive(Debug)]
pub struct ContainerNetwork {
    // might be good for debug
    #[allow(dead_code)]
    network_id: String,
    opts: NetworkCreateOptions,
    containers: HashMap<String, ContainerRunner>,
}

/// Options for adding a container
#[derive(Debug)]
pub enum AddContainerOptions {
    /// Use an already specified image to create the container
    Container(SuperImage),
    /// Use our [SuperDockerfile] construct
    DockerFile(SuperDockerfile),
    /// Use Bollard arguments with a tarball
    BollardArgs {
        image_options: bollard::image::BuildImageOptions<String>,
        tarball: Vec<u8>,
    },
}

/// Options for the API equivalent of `docker network create`
#[derive(Debug, Clone, Default)]
pub struct NetworkCreateOptions {
    pub name: String,
    pub driver: Option<String>,
    pub enable_ipv6: bool,
    pub options: HashMap<String, String>,
    pub labels: HashMap<String, String>,
    pub ipam: Ipam,
    /// If set the network will shutdown and start a new network if there is a
    /// name collision.
    pub overwrite_existing: bool,
    /// Configure an output directory for logging.
    pub output_dir_config: Option<OutputDirConfig>,
    /// If true, [ContainerCreateOptions] with `log_outs: None` will use
    /// this value as default
    pub log_by_default: bool,
    /// Turns on debug tracing
    pub debug: bool,
}

/// Configuration for things like the logging directory
///
/// Set the value as a temporary directory or an
/// gitignored directory in a repository. This will not mount the output
/// directory, it will create other directories and mount them.
///
/// This also adds environment variable
/// [CONTAINER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME] to the container. Add
/// outputs using the env var to ensure compatibility.
#[derive(Debug, Clone, Default)]
pub struct OutputDirConfig {
    /// Directory for dealing with outputs.
    pub output_dir: String,
    /// Write all captured output to a log file
    pub save_logs: bool,
}

/// Extra options related to `docker create`
#[derive(Debug, Clone, Default)]
pub struct ExtraAddContainerOptions {
    pub hostname: Option<String>,
    pub mac_address: Option<String>,
}

// TODO make the rest of `super_orchestrator` use tokio::signal::ctrl_c instead,
// but check if multiple `ctrl_c` can be used in parallel

// TODO make `cli_docker::ContainerNetwork` use more of the style used here

impl ContainerNetwork {
    /// Configures total teardown on ctrl-c. This also calls
    /// `std::process::exit(1);`
    pub fn teardown_on_ctrlc(&self) {
        let cn_name = self.opts.name.clone();
        tokio::task::spawn(async move {
            let span = tracing::span!(Level::INFO, "ctrlc handler");
            let _enter = span.enter();

            if tokio::signal::ctrl_c()
                .await
                .stack_err("Failed to wait for ctrlc")
                .is_err()
            {
                std::process::exit(1);
            }
            tracing::info!("ctrlc detected, tearing down networks");
            // also log to stdout because it's immediate
            eprintln!("ctrlc detected, tearing down networks");

            let _ = total_teardown(&cn_name, [])
                .await
                .stack()
                .inspect_err(|err| tracing::error!("{err}"))
                .in_current_span();

            std::process::exit(1);
        });
    }

    /// opts is a passthrough argument to [bollard::Docker::create_network]
    ///
    /// `opts.overwrite_existing` can be set to configure overwriting any
    /// existing network with the same name.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %opts.name,
        )
    )]
    pub async fn create(opts: NetworkCreateOptions) -> Result<Self> {
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
                if opts.debug {
                    tracing::debug!("Tearing down name match for {network_name} connection");
                }
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

        if opts.debug {
            tracing::info!(
                "network name: {}\n network id: {}\n message: {}",
                opts.name,
                response.id,
                response.warning
            );
        }

        Ok(Self {
            network_id: response.id,
            opts,
            containers: Default::default(),
        })
    }

    /// Adds the container to the network, but does not run it yet. It may call
    /// `docker build` if not already cached. This function will return
    /// error if the container is already registered in the network.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
            container.name = %container.name,
        )
    )]
    pub async fn add_container(
        &mut self,
        mut add_opts: AddContainerOptions,
        network_opts: ExtraAddContainerOptions,
        mut container: ContainerCreateOptions,
    ) -> Result<()> {
        if self.containers.contains_key(&container.name) {
            return Err("Name is already registered").stack();
        }

        if container.name.is_empty() {
            return Err("Name for container can't be empty").stack();
        }

        let output_dir = if let Some(ref output_config) = self.opts.output_dir_config {
            add_opts = AddContainerOptions::DockerFile(
                match add_opts {
                    AddContainerOptions::Container(image) => image.to_docker_file(),
                    AddContainerOptions::DockerFile(docker_file) => docker_file,
                    AddContainerOptions::BollardArgs {
                        image_options,
                        tarball,
                    } => SuperDockerfile::build_with_bollard_defaults(image_options, tarball)
                        .await
                        .stack()?
                        .0
                        .to_docker_file(),
                }
                .append_dockerfile_instructions(["RUN mkdir /super_out"]),
            );

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

            container.volumes.push((
                output_dir_str.to_string(),
                "/super_orchestrator_out".to_string(),
            ));
            container.env_vars.push(format!(
                "{CONTAINER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME}=/super_orchestrator_out"
            ));

            Some(output_dir)
        } else {
            None
        };

        let image = match add_opts {
            AddContainerOptions::Container(image) => image,
            AddContainerOptions::DockerFile(docker_file) => {
                docker_file
                    .build_image()
                    .await
                    .stack_err("ContainerNetwork::add_container")?
                    .0
            }
            AddContainerOptions::BollardArgs {
                image_options,
                tarball,
            } => {
                SuperDockerfile::build_with_bollard_defaults(image_options, tarball)
                    .await
                    .stack_err("ContainerNetwork::add_container")?
                    .0
            }
        };

        self.containers
            .insert(container.name.clone(), ContainerRunner {
                should_be_started: false,
                image,
                container_opts: container,
                network_opts,
                stdin: None,
                wait_container: None,
                output_dir,
                debug: self.opts.debug,
            });

        Ok(())
    }

    /// Calls the API equivalent of `docker create` and `docker start` on the
    /// container using its options.
    ///
    /// This will also mark the container with flag `should_be_started` to true.
    /// If this flag is set for the container, future docker commands won't
    /// be executed.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
            container.name = %container_name,
        )
    )]
    pub async fn start_container(&mut self, container_name: &str) -> Result<()> {
        let Some(container_runner) = self.containers.get_mut(container_name) else {
            return Err(format!("Container with name {container_name} not found")).stack();
        };

        container_runner
            .start_container(
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

    /// Calls [ContainerNetwork::start_container] for all registered containers.
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
            .map(|container_runner| {
                Box::pin(
                    container_runner.start_container(
                        network_name.clone(),
                        self.opts.log_by_default,
                        self.opts
                            .output_dir_config
                            .as_ref()
                            .is_some_and(|config| config.save_logs),
                    ),
                )
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

    async fn wait_with_timeout_internal(
        &mut self,
        mut names: Vec<String>,
        terminate_on_failure: bool,
        duration: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        let mut skip_fail = true;
        // we will check in a loop so that if a container has failed in the meantime, we
        // terminate all
        let mut i = 0;
        loop {
            if names.is_empty() {
                break
            }
            if i >= names.len() {
                i = 0;
                let current = Instant::now();
                let elapsed = current.saturating_duration_since(start);
                if elapsed > duration {
                    if skip_fail {
                        // give one extra round, this is strong enough for the `Duration::ZERO`
                        // guarantee
                        skip_fail = false;
                    } else {
                        if terminate_on_failure {
                            // we put in some extra delay so that the log file writers have some
                            // extra time to finish
                            sleep(Duration::from_millis(200)).await;
                            self.teardown().await.stack()?;
                        }
                        return Err(Error::timeout().add_err_locationless(format!(
                            "ContainerNetwork::wait_with_timeout timeout waiting for container \
                             names {names:?} to complete"
                        )))
                    }
                } else {
                    sleep(Duration::from_millis(200)).await;
                }
            }

            let name = &names[i];
            let state = self.containers.get_mut(name).unwrap();
            if let Some(wait_container) = state.wait_container.as_mut() {
                select! {
                    item = wait_container.next() => {
                        match item {
                            Some(item) => {
                                match item {
                                    Ok(response) => {
                                        // nonzero status codes seem to always manifest as a
                                        // bollard error instead, but have this just in case
                                        if response.status_code != 0 {
                                            //TODO
                                        }
                                        dbg!(response.status_code);
                                        dbg!(response.error);
                                        names.remove(i);
                                    },
                                    Err(bollard_err) => {
                                        //let _ = self.teardown().await;
                                        //todo!();
                                        dbg!(bollard_err);
                                        names.remove(i);
                                    },
                                }
                            },
                            None => {
                                // has already been accessed and terminated

                                if self.opts.debug {
                                    tracing::debug!("wait_with_timeout_internal: assuming already \
                                        accessed and terminated");
                                }
                                names.remove(i);
                            },
                        }
                    }
                    _ = sleep(Duration::from_millis(100)) => {
                        // continue
                        i += 1;
                    }
                }
            } else {
                // not active

                if self.opts.debug {
                    tracing::debug!("wait_with_timeout_internal: assuming never active");
                }
                names.remove(i);
            }
        }
        Ok(())
    }

    /// Waits for all containers marked as "important" to shutdown
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
        )
    )]
    pub async fn wait_important(&mut self) -> Result<()> {
        let importants = self
            .containers
            .iter()
            .filter(|(_, container)| container.container_opts.important)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();

        if self.opts.debug {
            tracing::debug!("total important: {}", importants.len());
        }

        self.wait_with_timeout_internal(importants, true, Duration::MAX)
            .await
            .stack()
    }

    /// Gets the stdin of the container, which should exist after the container
    /// is started.
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

    /// Stop all containers and delete the network. If an error is returned, it
    /// still should have stopped all containers and deleted the network unless
    /// there was some API call issue.
    #[tracing::instrument(skip_all,
        fields(
            network.name = %self.opts.name,
        )
    )]
    pub async fn teardown(&mut self) -> Result<()> {
        total_teardown(&self.opts.name, self.containers.drain().map(|(key, _)| key))
            .await
            .stack()
    }

    /// Wait for all listed containers to be healthy.
    ///
    /// If a container does not have a healthcheck, it is automatically
    /// considered healthy.
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
