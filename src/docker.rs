use std::{collections::BTreeMap, time::Duration};

use log::warn;
use tokio::time::Instant;

use crate::{
    acquire_dir_path, acquire_file_path, Command, CommandResult, CommandRunner, LogFileOptions,
    MapAddError, Result,
};

/// Container running information, put this into a `ContainerNetwork`
#[derive(Debug)]
pub struct Container {
    pub name: String,
    pub dockerfile: Option<String>,
    // if `dockerfile` is not set, this should be an existing image name, otherwise this becomes
    // the name of the build image
    pub image: String,
    // each string is passed in as `--build-arg "[String]"` (the quotations are added), so a string
    // "ARG=val" would set the variable "ARG" for the docker file to use.
    pub build_args: Vec<String>,
    // path to the entrypoint binary locally
    pub entrypoint_path: String,
    // passed in as ["arg1", "arg2", ...] with the bracket and quotations being added
    pub entrypoint_args: Vec<String>,
}

impl Container {
    pub fn new(
        name: &str,
        dockerfile: Option<&str>,
        image: &str,
        build_args: &[&str],
        entrypoint_path: &str,
        entrypoint_args: &[&str],
    ) -> Self {
        Self {
            name: name.to_owned(),
            dockerfile: dockerfile.map(|s| s.to_owned()),
            image: image.to_owned(),
            build_args: build_args.iter().fold(Vec::new(), |mut acc, e| {
                acc.push(e.to_string());
                acc
            }),
            entrypoint_path: entrypoint_path.to_owned(),
            entrypoint_args: entrypoint_args.iter().fold(Vec::new(), |mut acc, e| {
                acc.push(e.to_string());
                acc
            }),
        }
    }
}

/// A complete network of one or more containers, a more programmable
/// alternative to `docker-compose`
#[must_use]
#[derive(Debug)]
pub struct ContainerNetwork {
    network_name: String,
    containers: Vec<Container>,
    /// is `--internal` by default
    is_not_internal: bool,
    log_dir: String,
    active_container_ids: BTreeMap<String, String>,
    container_runners: BTreeMap<String, CommandRunner>,
    container_results: BTreeMap<String, CommandResult>,
}

impl Drop for ContainerNetwork {
    fn drop(&mut self) {
        if !self.container_runners.is_empty() {
            warn!(
                "`ContainerNetwork` \"{}\" was dropped with internal container runners still \
                 running. If not consumed properly then the internal commands may continue using \
                 up resources or be force stopped at any time",
                self.network_name
            )
        }
    }
}

impl ContainerNetwork {
    pub fn new(
        network_name: &str,
        containers: Vec<Container>,
        is_not_internal: bool,
        log_dir: &str,
    ) -> Self {
        Self {
            network_name: network_name.to_owned(),
            containers,
            is_not_internal,
            log_dir: log_dir.to_owned(),
            active_container_ids: BTreeMap::new(),
            container_runners: BTreeMap::new(),
            container_results: BTreeMap::new(),
        }
    }

    pub fn get_ids(&self) -> Vec<String> {
        self.active_container_ids.keys().cloned().collect()
    }

    // just apply `rm -f` to all containers, ignoring errors
    async fn unconditional_terminate(&mut self) {
        while let Some((_, id)) = self.active_container_ids.pop_first() {
            let _ = Command::new("docker", &["rm", "-f", &id])
                .run_to_completion()
                .await;
        }
    }

    /// Force removes all containers
    pub async fn terminate_all(mut self) -> Result<()> {
        while let Some(entry) = self.active_container_ids.first_entry() {
            let comres = Command::new("docker", &["rm", "-f", entry.get()])
                .run_to_completion()
                .await
                .map_add_err(|| "ContainerNetwork::terminate_all()");
            if let Err(e) = comres {
                // in case this is some weird one-off problem, we do not want to leave a whole
                // network running
                self.unconditional_terminate().await;
                return Err(e)
            }
            // ignore status failures, because the container is probably already gone
            // TODO there is maybe some error message parsing we should do

            // only pop from `container_ids` after success
            self.active_container_ids.pop_first().unwrap();
        }
        Ok(())
    }

    pub async fn run(&mut self, ci_mode: bool) -> Result<()> {
        // preverification to prevent much more expensive later container creation undos
        let log_dir = acquire_dir_path(&self.log_dir)
            .await?
            .to_str()
            .map_add_err(|| {
                format!(
                    "ContainerNetwork::run() -> log_dir: \"{}\" could not be canonicalized into a \
                     String",
                    self.log_dir
                )
            })?
            .to_owned();
        let mut debug_log = LogFileOptions {
            directory: log_dir.clone(),
            file_name: format!("container_network_{}.log", self.network_name),
            create: true,
            overwrite: true,
        };
        // precheck and overwrite
        let _ = debug_log.acquire_file().await?;
        // settings we will use for the rest
        debug_log.create = false;
        debug_log.overwrite = false;
        let debug_log = Some(debug_log);
        for container in &self.containers {
            acquire_file_path(&container.entrypoint_path).await?;
            if let Some(ref dockerfile) = container.dockerfile {
                acquire_file_path(dockerfile).await?;
            }
            // remove potentially previously existing container with same name
            let _ = Command::new("docker", &["rm", &container.name])
                // never put in CI mode or put in debug file, error on nonexistent container is
                // confusing, actual errors will be returned
                .ci_mode(false)
                .run_to_completion()
                .await?;
        }

        // remove old network if it exists (there is no option to ignore nonexistent
        // networks, drop exit status errors and let the creation command handle any
        // higher order errors)
        let _ = Command::new("docker", &["network", "rm", &self.network_name])
            .ci_mode(ci_mode)
            .stdout_log(&debug_log)
            .stderr_log(&debug_log)
            .run_to_completion()
            .await;
        let comres = if self.is_not_internal {
            Command::new("docker", &["network", "create", &self.network_name])
                .ci_mode(ci_mode)
                .stdout_log(&debug_log)
                .stderr_log(&debug_log)
                .run_to_completion()
                .await?
        } else {
            Command::new("docker", &[
                "network",
                "create",
                "--internal",
                &self.network_name,
            ])
            .ci_mode(ci_mode)
            .stdout_log(&debug_log)
            .stderr_log(&debug_log)
            .run_to_completion()
            .await?
        };
        // TODO we can get the network id
        comres.assert_success()?;

        // run all the creation first so that everything is pulled and prepared
        for container in &self.containers {
            if let Some(ref dockerfile) = container.dockerfile {
                let mut dockerfile = acquire_file_path(dockerfile).await?;
                // yes we do need to do this because of the weird way docker build works
                let dockerfile_full = dockerfile.to_str().unwrap().to_owned();
                let mut args = vec!["build", "-t", &container.image, "--file", &dockerfile_full];
                dockerfile.pop();
                let dockerfile_dir = dockerfile.to_str().unwrap().to_owned();
                // TODO
                let mut tmp = vec![];
                for arg in &container.build_args {
                    tmp.push(format!("\"{arg}\""));
                }
                for s in &tmp {
                    args.push(s);
                }
                args.push(&dockerfile_dir);
                Command::new("docker", &args)
                    .ci_mode(ci_mode)
                    .stdout_log(&debug_log)
                    .stderr_log(&debug_log)
                    .run_to_completion()
                    .await?
                    .assert_success()?;
            }

            let bin_path = acquire_file_path(&container.entrypoint_path).await?;
            let bin_s = bin_path.file_name().unwrap().to_str().unwrap();
            // just include the needed binary
            let volume = format!("{}:/usr/bin/{}", container.entrypoint_path, bin_s);
            let mut args = vec![
                "create",
                "--rm",
                "--network",
                &self.network_name,
                "--hostname",
                &container.name,
                "--name",
                &container.name,
                "--volume",
                &volume,
                "-t",
                &container.image,
            ];
            args.push(bin_s);
            // TODO
            let mut tmp = vec![];
            for arg in &container.entrypoint_args {
                tmp.push(format!("\"{arg}\""));
            }
            for s in &tmp {
                args.push(s);
            }
            /*if !container.entrypoint_args.is_empty() {
                let mut s = "[";

                for (i, arg) in container.entrypoint_args.iter().enumerate() {
                    args += "\"";
                    args += "\"";
                }
                args.push(&container.entrypoint_args);
                s += "]";
            }*/
            match Command::new("docker", &args)
                .ci_mode(ci_mode)
                .stdout_log(&debug_log)
                .stderr_log(&debug_log)
                .run_to_completion()
                .await
            {
                Ok(output) => {
                    match output.assert_success() {
                        Ok(_) => {
                            let mut id = output.stdout;
                            // remove trailing '\n'
                            id.pop().unwrap();
                            self.active_container_ids.insert(container.name.clone(), id);
                        }
                        Err(e) => {
                            self.unconditional_terminate().await;
                            return Err(e)
                        }
                    }
                }
                Err(e) => {
                    self.unconditional_terminate().await;
                    return e.map_add_err(|| "{self:?}.run()")
                }
            }
        }

        // start all containers
        for (container_name, id) in self.active_container_ids.clone().iter() {
            let mut command = Command::new("docker", &["start", "--attach", id]);
            command.stdout_log = Some(LogFileOptions {
                directory: log_dir.clone(),
                file_name: format!("container_{}_stdout.log", container_name),
                create: true,
                overwrite: true,
            });
            command.stderr_log = Some(LogFileOptions {
                directory: log_dir.clone(),
                file_name: format!("container_{}_stderr.log", container_name),
                create: true,
                overwrite: true,
            });
            match command.ci_mode(ci_mode).run().await {
                Ok(runner) => {
                    self.container_runners
                        .insert(container_name.clone(), runner);
                }
                Err(e) => {
                    self.unconditional_terminate().await;
                    return Err(e)
                }
            }
        }

        Ok(())
    }

    /// Returns `Err(timed_out_id)` on timeout, `Ok(Err(..))` on internal error,
    /// `Ok(Ok(()))` on success in waiting for all containers to stop
    pub async fn wait_with_timeout(
        &mut self,
        mut ids_to_wait_on: Vec<String>,
        duration: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        let mut current = start;
        while let Some(id) = ids_to_wait_on.pop() {
            let runner = self.container_runners.get_mut(&id).map_add_err(|| {
                "ContainerNetwork::wait_timeout -> id \"{id}\" not found in the network"
            })?;
            let elapsed = current.saturating_duration_since(start);
            if let Err(e) = runner
                .wait_with_timeout(duration.checked_sub(elapsed).unwrap_or(Duration::ZERO))
                .await
            {
                if e.is_timeout() {
                    return e.map_add_err(|| {
                        format!(
                            "ContainerNetwork::wait_timeout() timeout waiting for container id \
                             \"{id}\" to complete"
                        )
                    })
                } else {
                    self.active_container_ids.remove(&id).unwrap();
                    return e.map_add_err(|| {
                        format!(
                            "ContainerNetwork::wait_timeout() command runner error with container \
                             id \"{id}\""
                        )
                    })
                }
            }
            self.active_container_ids.remove(&id).unwrap();
            let runner = self.container_runners.remove(&id).unwrap();
            self.container_results
                .insert(id.clone(), runner.get_command_result().unwrap());
            current = Instant::now();
        }
        Ok(())
    }
}
