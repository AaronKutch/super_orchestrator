use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use log::warn;
use tokio::time::{sleep, Instant};

use crate::{
    acquire_file_path, acquire_path, Command, CommandResult, CommandRunner, FileOptions,
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
    // note that the binary is automatically included
    pub volumes: Vec<(String, String)>,
    // path to the entrypoint binary locally
    pub entrypoint_path: String,
    // passed in as ["arg1", "arg2", ...] with the bracket and quotations being added
    pub entrypoint_args: Vec<String>,
}

impl Container {
    /// Note: `name` is also used for the hostname
    pub fn new(
        name: &str,
        dockerfile: Option<&str>,
        image: Option<&str>,
        build_args: &[&str],
        volumes: &[(&str, &str)],
        entrypoint_path: &str,
        entrypoint_args: &[&str],
    ) -> Self {
        Self {
            name: name.to_owned(),
            dockerfile: dockerfile.map(|s| s.to_owned()),
            image: image.map(|s| s.to_owned()).unwrap_or(name.to_owned()),
            build_args: build_args.iter().fold(Vec::new(), |mut acc, e| {
                acc.push(e.to_string());
                acc
            }),
            volumes: volumes.iter().fold(Vec::new(), |mut acc, e| {
                acc.push((e.0.to_string(), e.1.to_string()));
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
///
/// Note: when having multiple containers on some platforms there is
/// an obnoxious issue <https://github.com/moby/libnetwork/issues/2647>
/// that means you may have to set `is_not_internal`
#[must_use]
#[derive(Debug)]
pub struct ContainerNetwork {
    network_name: String,
    containers: BTreeMap<String, Container>,
    /// is `--internal` by default
    is_not_internal: bool,
    log_dir: String,
    active_container_ids: BTreeMap<String, String>,
    container_runners: BTreeMap<String, CommandRunner>,
    pub container_results: BTreeMap<String, CommandResult>,
    // true if the network has been recreated
    network_recreated: bool,
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
    /// Can return `Err` if there are containers with duplicate names
    pub fn new(
        network_name: &str,
        containers: Vec<Container>,
        is_not_internal: bool,
        log_dir: &str,
    ) -> Result<Self> {
        let mut map = BTreeMap::new();
        for container in containers {
            if map.contains_key(&container.name) {
                return Err(format!(
                    "ContainerNetwork::new() two containers were supplied with the same name \
                     \"{}\"",
                    container.name
                ))
                .map_add_err(|| ())
            }
            map.insert(container.name.clone(), container);
        }
        Ok(Self {
            network_name: network_name.to_owned(),
            containers: map,
            is_not_internal,
            log_dir: log_dir.to_owned(),
            active_container_ids: BTreeMap::new(),
            container_runners: BTreeMap::new(),
            container_results: BTreeMap::new(),
            network_recreated: false,
        })
    }

    pub fn get_active_container_ids(&self) -> &BTreeMap<String, String> {
        &self.active_container_ids
    }

    /// Get the names of all active containers
    pub fn active_names(&self) -> Vec<String> {
        self.active_container_ids.keys().cloned().collect()
    }

    /// Get the names of all inactive containers
    pub fn inactive_names(&self) -> Vec<String> {
        let mut v = vec![];
        for name in self.containers.keys() {
            if !self.active_container_ids.contains_key(name) {
                v.push(name.clone());
            }
        }
        v
    }

    /// Force removes containers with the given names. No errors are returned in
    /// case of duplicate names or names that are not in the active set.
    pub async fn terminate(&mut self, names: &[&str]) {
        for name in names {
            if let Some(docker_id) = self.active_container_ids.remove(*name) {
                // TODO we should parse errors to differentiate whether it is
                // simply a race condition where the container finished before
                // this time, or is a proper command runner error.
                let _ = Command::new("docker rm -f", &[&docker_id])
                    .run_to_completion()
                    .await;
                let mut runner = self.container_runners.remove(*name).unwrap();
                let _ = runner.terminate().await;
            }
        }
    }

    /// Force removes all active containers.
    pub async fn terminate_all(&mut self) {
        while let Some((_, id)) = self.active_container_ids.pop_first() {
            let _ = Command::new("docker", &["rm", "-f", &id])
                .run_to_completion()
                .await;
        }
        while let Some((_, mut runner)) = self.container_runners.pop_first() {
            let _ = runner.terminate().await;
        }
    }

    /// Runs only the given `names`
    pub async fn run(&mut self, names: &[&str], ci_mode: bool) -> Result<()> {
        // relatively cheap preverification should be done first to prevent much more
        // expensive later undos
        let mut set = BTreeSet::new();
        for name in names {
            if set.contains(name) {
                return Err(format!(
                    "ContainerNetwork::run() two containers were supplied with the same name \
                     \"{name}\""
                ))
                .map_add_err(|| ())
            }
            if !self.containers.contains_key(*name) {
                return Err(format!(
                    "ContainerNetwork::run() argument name \"{name}\" is not contained in the \
                     network"
                ))
                .map_add_err(|| ())
            }
            set.insert(*name);
        }

        for name in names {
            let container = &self.containers[*name];
            acquire_file_path(&container.entrypoint_path).await?;
            if let Some(ref dockerfile) = container.dockerfile {
                acquire_file_path(dockerfile).await?;
            }
        }

        let debug_log = FileOptions::write2(
            &self.log_dir,
            &format!("container_network_{}.log", self.network_name),
        );
        // prechecking the log directory
        debug_log
            .preacquire()
            .await
            .map_add_err(|| "ContainerNetwork::run() when acquiring logs directory")?;

        // do this last
        for name in names {
            // remove potentially previously existing container with same name
            let _ = Command::new("docker rm -f", &[name])
                // never put in CI mode or put in debug file, error on nonexistent container is
                // confusing, actual errors will be returned
                .ci_mode(false)
                .run_to_completion()
                .await?;
        }

        if !self.network_recreated {
            // remove old network if it exists (there is no option to ignore nonexistent
            // networks, drop exit status errors and let the creation command handle any
            // higher order errors)
            let _ = Command::new("docker network rm", &[&self.network_name])
                .ci_mode(ci_mode)
                .stdout_log(&debug_log)
                .stderr_log(&debug_log)
                .run_to_completion()
                .await;
            let comres = if self.is_not_internal {
                Command::new("docker network create", &[&self.network_name])
                    .ci_mode(ci_mode)
                    .stdout_log(&debug_log)
                    .stderr_log(&debug_log)
                    .run_to_completion()
                    .await?
            } else {
                Command::new("docker network create --internal", &[&self.network_name])
                    .ci_mode(ci_mode)
                    .stdout_log(&debug_log)
                    .stderr_log(&debug_log)
                    .run_to_completion()
                    .await?
            };
            // TODO we can get the network id
            comres.assert_success()?;
            self.network_recreated = true;
        }

        // run all the creation first so that everything is pulled and prepared
        for name in names {
            let container = &self.containers[*name];
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
            let mut args = vec![
                "create",
                "--rm",
                "--network",
                &self.network_name,
                "--hostname",
                name,
                "--name",
                name,
            ];
            // volumes
            let mut volumes = container.volumes.clone();
            // include the needed binary
            volumes.push((
                container.entrypoint_path.clone(),
                format!("/usr/bin/{bin_s}"),
            ));
            let mut combined_volumes = vec![];
            for volume in &volumes {
                let path = acquire_path(&volume.0)
                    .await
                    .map_add_err(|| "could not locate local part of volume argument")?;
                combined_volumes.push(format!(
                    "{}:{}",
                    path.to_str().map_add_err(|| ())?,
                    volume.1
                ));
            }
            for volume in &combined_volumes {
                args.push("--volume");
                args.push(volume);
            }
            args.push("-t");
            args.push(&container.image);
            // the binary
            args.push(bin_s);
            // TODO
            let mut tmp = vec![];
            for arg in &container.entrypoint_args {
                tmp.push(arg.to_owned());
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
            let command = Command::new("docker", &args)
                .ci_mode(ci_mode)
                .stdout_log(&debug_log)
                .stderr_log(&debug_log);
            if ci_mode {
                println!("ci_mode command debug: {command:#?}");
            }
            match command.run_to_completion().await {
                Ok(output) => {
                    match output.assert_success() {
                        Ok(_) => {
                            let mut docker_id = output.stdout;
                            // remove trailing '\n'
                            docker_id.pop().unwrap();
                            self.active_container_ids
                                .insert(name.to_string(), docker_id);
                        }
                        Err(e) => {
                            self.terminate_all().await;
                            return Err(e)
                        }
                    }
                }
                Err(e) => {
                    self.terminate_all().await;
                    return e.map_add_err(|| "{self:?}.run()")
                }
            }
        }

        // start containers
        for name in names {
            let docker_id = &self.active_container_ids[*name];
            let command = Command::new("docker start --attach", &[docker_id])
                .stdout_log(&FileOptions::write2(
                    &self.log_dir,
                    &format!("container_{}_stdout.log", name),
                ))
                .stderr_log(&FileOptions::write2(
                    &self.log_dir,
                    &format!("container_{}_stderr.log", name),
                ));
            match command.ci_mode(ci_mode).run().await {
                Ok(runner) => {
                    self.container_runners.insert(name.to_string(), runner);
                }
                Err(e) => {
                    self.terminate_all().await;
                    return Err(e)
                }
            }
        }

        Ok(())
    }

    pub async fn run_all(&mut self, ci_mode: bool) -> Result<()> {
        let names = self.inactive_names();
        let mut v: Vec<&str> = vec![];
        for name in &names {
            v.push(name);
        }
        self.run(&v, ci_mode).await.map_add_err(|| ())
    }

    /// If `terminate_on_failure`, then if any container runner has an error or
    /// completes with unsuccessful return status, the whole network will be
    /// terminated.
    ///
    /// If called with `Duration::ZERO`, this will complete successfully if all
    /// containers were terminated before this call.
    pub async fn wait_with_timeout(
        &mut self,
        names: &mut Vec<String>,
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
                            self.terminate_all().await;
                        }
                        return format!(
                            "ContainerNetwork::wait_with_timeout() timeout waiting for container \
                             names {names:?} to complete"
                        )
                        .map_add_err(|| ())
                    }
                } else {
                    sleep(Duration::from_millis(256)).await;
                }
            }

            let name = &names[i];
            let runner = self.container_runners.get_mut(name).map_add_err(|| {
                "ContainerNetwork::wait_with_timeout -> name \"{name}\" not found in the network"
            })?;
            match runner.wait_with_timeout(Duration::ZERO).await {
                Ok(()) => {
                    self.active_container_ids.remove(name).unwrap();
                    let runner = self.container_runners.remove(name).unwrap();
                    let res = runner.get_command_result().unwrap();
                    let status = res.assert_success();
                    self.container_results.insert(name.clone(), res);
                    if terminate_on_failure && status.is_err() {
                        self.terminate_all().await;
                        return status.map_add_err(|| {
                            format!(
                                "ContainerNetwork::wait_with_timeout() command runner had \
                                 unsuccessful return status with container id \"{name}\""
                            )
                        })
                    }
                    names.remove(i);
                }
                Err(e) => {
                    if !e.is_timeout() {
                        self.active_container_ids.remove(name).unwrap();
                        if terminate_on_failure {
                            self.terminate_all().await;
                        }
                        return e.map_add_err(|| {
                            format!(
                                "ContainerNetwork::wait_with_timeout() command runner error with \
                                 container name \"{name}\""
                            )
                        })
                    }
                    i += 1;
                }
            }
        }
        Ok(())
    }

    pub async fn wait_with_timeout_all(
        &mut self,
        terminate_on_failure: bool,
        duration: Duration,
    ) -> Result<()> {
        let mut names = self.active_names();
        self.wait_with_timeout(&mut names, terminate_on_failure, duration)
            .await
    }
}
