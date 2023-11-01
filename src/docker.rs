//! Functions for managing Docker containers
//!
//! See the `docker_entrypoint_pattern` example for how to use all of this
//! together.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use log::{info, warn};
use stacked_errors::{Error, Result, StackableErr};
use tokio::time::{sleep, Instant};
use uuid::Uuid;

use crate::{
    acquire_dir_path, acquire_file_path, acquire_path, Command, CommandResult, CommandRunner,
    FileOptions,
};

/// Ways of using a dockerfile for building a container
#[derive(Debug, Clone)]
pub enum Dockerfile {
    /// Builds using an image in the format "name:tag" such as "fedora:38"
    /// (running will call something such as `docker pull name:tag`)
    NameTag(String),
    /// Builds from a dockerfile on a path (e.x.
    /// "./tests/dockerfiles/example.dockerfile")
    Path(String),
    /// Builds from contents that are written to "__tmp.dockerfile" in a
    /// directory determined by the `ContainerNetwork`. Note that resources used
    /// by this dockerfile may need to be in the same directory.
    Contents(String),
}

/// Container running information, put this into a `ContainerNetwork`
#[derive(Debug, Clone)]
pub struct Container {
    /// The name of the container, note the "name:tag" docker argument would go
    /// in [Dockerfile::NameTag]
    pub name: String,
    /// Usually should be the same as the tag
    pub host_name: String,
    /// If true, `host_name` is used directly without appending a UUID
    pub no_uuid_for_host_name: bool,
    /// The dockerfile arguments
    pub dockerfile: Dockerfile,
    /// Any flags and args passed to to `docker build`
    pub build_args: Vec<String>,
    /// Any flags and args passed to to `docker create`. Volumes by `volumes
    /// have the advantage of being canonicalized and prechecked.
    pub create_args: Vec<String>,
    /// note that the entrypoint binary is automatically included if
    /// `extrypoint_path` is set
    pub volumes: Vec<(String, String)>,
    /// Environment variable pairs passed to docker
    pub environment_vars: Vec<(String, String)>,
    /// Path to the entrypoint binary locally
    pub entrypoint_path: Option<String>,
    /// passed in as ["arg1", "arg2", ...] with the bracket and quotations being
    /// added
    pub entrypoint_args: Vec<String>,
}

impl Container {
    /// Creates the information needed to describe a `Container`. `name` is used
    /// for both the `name` and `hostname`.
    pub fn new(
        name: &str,
        dockerfile: Dockerfile,
        entrypoint_path: Option<&str>,
        entrypoint_args: &[&str],
    ) -> Self {
        Self {
            name: name.to_owned(),
            host_name: name.to_owned(),
            no_uuid_for_host_name: false,
            dockerfile,
            build_args: vec![],
            create_args: vec![],
            volumes: vec![],
            environment_vars: vec![],
            entrypoint_path: entrypoint_path.map(|s| s.to_owned()),
            entrypoint_args: entrypoint_args.iter().fold(Vec::new(), |mut acc, e| {
                acc.push(e.to_string());
                acc
            }),
        }
    }

    pub fn volumes(mut self, volumes: &[(&str, &str)]) -> Self {
        self.volumes = volumes.iter().fold(Vec::new(), |mut acc, e| {
            acc.push((e.0.to_string(), e.1.to_string()));
            acc
        });
        self
    }

    /// Sets the `build_args`
    pub fn build_args(mut self, build_args: &[&str]) -> Self {
        self.build_args = build_args.iter().fold(Vec::new(), |mut acc, e| {
            acc.push(e.to_string());
            acc
        });
        self
    }

    /// Sets the `create_args`
    pub fn create_args(mut self, create_args: &[&str]) -> Self {
        self.create_args = create_args.iter().fold(Vec::new(), |mut acc, e| {
            acc.push(e.to_string());
            acc
        });
        self
    }

    pub fn environment_vars(mut self, environment_vars: &[(&str, &str)]) -> Self {
        self.environment_vars = environment_vars.iter().fold(Vec::new(), |mut acc, e| {
            acc.push((e.0.to_string(), e.1.to_string()));
            acc
        });
        self
    }

    pub fn entrypoint_args(mut self, entrypoint_args: &[&str]) -> Self {
        self.entrypoint_args = entrypoint_args.iter().fold(Vec::new(), |mut acc, e| {
            acc.push(e.to_string());
            acc
        });
        self
    }

    /// Turns of the default behavior of attaching the UUID to the hostname
    pub fn no_uuid_for_host_name(mut self) -> Self {
        self.no_uuid_for_host_name = true;
        self
    }
}

/// A complete network of one or more containers, a more programmable
/// alternative to `docker-compose`
///
/// # Note
///
/// When running multiple containers with networking, there is an issue on some
/// platforms <https://github.com/moby/libnetwork/issues/2647> that means you
/// may have to set `is_not_internal` to `true` even if networking is only done
/// between containers within the network.
#[must_use]
#[derive(Debug)]
pub struct ContainerNetwork {
    uuid: Uuid,
    network_name: String,
    containers: BTreeMap<String, Container>,
    dockerfile_write_dir: Option<String>,
    /// is `--internal` by default
    is_not_internal: bool,
    log_dir: String,
    active_container_ids: BTreeMap<String, String>,
    container_runners: BTreeMap<String, CommandRunner>,
    pub container_results: BTreeMap<String, Result<CommandResult>>,
    network_active: bool,
}

impl Drop for ContainerNetwork {
    fn drop(&mut self) {
        // we purposely order in this way to avoid calling `panicking` in the
        // normal case
        if !self.container_runners.is_empty() {
            if !std::thread::panicking() {
                warn!(
                    "`ContainerNetwork` \"{}\" was dropped with internal container runners still \
                     running (a termination function needs to be called",
                    self.network_name_with_uuid()
                )
            }
        } else if self.network_active && (!std::thread::panicking()) {
            // we can't call async/await in a `drop` function, and it would be suspicious
            // anyway
            warn!(
                "`ContainerNetwork` \"{}\" was dropped with the network still active \
                 (`ContainerNetwork::terminate_all` needs to be called)",
                self.network_name_with_uuid()
            )
        }
    }
}

impl ContainerNetwork {
    /// Creates a new `ContainerNetwork`.
    ///
    /// This function generates a `Uuid` used for enabling multiple
    /// `ContainerNetwork`s with the same names and ids to run simultaneously.
    /// The uuid is appended to network names, container names, and hostnames.
    /// Arguments involving container names automatically append the uuid.
    ///
    /// `network_name` sets the name of the docker network that containers will
    /// be attached to, `containers` is the set of containers that can be
    /// referred to later by name, `dockerfile_write_dir` is the directory in
    /// which "__tmp.dockerfile" can be written if `Dockerfile::Contents` is
    /// used, `is_not_internal` turns off `--internal`, and `log_dir` is where
    /// ".log" log files will be written.
    ///
    /// Note: if `Dockerfile::Contents` is used, and if it uses resources like
    /// `COPY --from [resource]`, then the resource needs to be in
    /// `dockerfile_write_dir` because of restrictions that Docker makes.
    ///
    /// The standard layout is to have a "./logs" directory for the log files,
    /// "./dockerfiles" for the write directory, and
    /// "./dockerfiles/dockerfile_resources" for resources used by the
    /// dockerfiles.
    ///
    /// # Errors
    ///
    /// Can return an error if there are containers with duplicate names, or a
    /// container is built with `Dockerfile::Content` but no
    /// `dockerfile_write_dir` is specified.
    pub fn new(
        network_name: &str,
        containers: Vec<Container>,
        dockerfile_write_dir: Option<&str>,
        is_not_internal: bool,
        log_dir: &str,
    ) -> Result<Self> {
        let mut map = BTreeMap::new();
        for container in containers {
            if dockerfile_write_dir.is_none()
                && matches!(container.dockerfile, Dockerfile::Contents(_))
            {
                return Err(Error::from(
                    "ContainerNetwork::new() a container is built with `Dockerfile::Contents`, \
                     but `dockerfile_write_dir` is unset",
                ))
            }
            if map.contains_key(&container.name) {
                return Err(Error::from(format!(
                    "ContainerNetwork::new() two containers were supplied with the same name \
                     \"{}\"",
                    container.name
                )))
            }
            map.insert(container.name.clone(), container);
        }
        Ok(Self {
            uuid: Uuid::new_v4(),
            network_name: network_name.to_owned(),
            containers: map,
            dockerfile_write_dir: dockerfile_write_dir.map(|s| s.to_owned()),
            is_not_internal,
            log_dir: log_dir.to_owned(),
            active_container_ids: BTreeMap::new(),
            container_runners: BTreeMap::new(),
            container_results: BTreeMap::new(),
            network_active: false,
        })
    }

    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    pub fn uuid_as_string(&self) -> String {
        self.uuid.to_string()
    }

    pub fn network_name_with_uuid(&self) -> String {
        format!("{}_{}", self.network_name, self.uuid)
    }

    /// Returns an error if the container with the name could not be found
    pub fn container_name_with_uuid(&self, container_name: &str) -> Result<String> {
        if let Some(container) = self.containers.get(container_name) {
            Ok(format!("{}_{}", container.name, self.uuid))
        } else {
            Err(Error::from(format!(
                "container_name_with_uuid({container_name}): could not find container with given \
                 name"
            )))
        }
    }

    /// If `no_uuid_for_host_name` is true for the container, this just returns
    /// the `host_name` Returns an error if the container with the name
    /// could not be found
    pub fn hostname_with_uuid(&self, container_name: &str) -> Result<String> {
        if let Some(container) = self.containers.get(container_name) {
            if container.no_uuid_for_host_name {
                Ok(container.host_name.clone())
            } else {
                Ok(format!("{}_{}", container.host_name, self.uuid))
            }
        } else {
            Err(Error::from(format!(
                "hostname_with_uuid({container_name}): could not find container with given name"
            )))
        }
    }

    pub fn add_container(&mut self, container: Container) -> Result<&mut Self> {
        if self.dockerfile_write_dir.is_none()
            && matches!(container.dockerfile, Dockerfile::Contents(_))
        {
            return Err(Error::from(
                "ContainerNetwork::new() a container is built with `Dockerfile::Contents`, but \
                 `dockerfile_write_dir` is unset",
            ))
        }
        if self.containers.contains_key(&container.name) {
            return Err(Error::from(format!(
                "ContainerNetwork::new() two containers were supplied with the same name \"{}\"",
                container.name
            )))
        }
        self.containers.insert(container.name.clone(), container);
        Ok(self)
    }

    /// Adds the volumes to every container
    pub fn add_common_volumes(&mut self, volumes: &[(&str, &str)]) -> &mut Self {
        for container in self.containers.values_mut() {
            container
                .volumes
                .extend(volumes.iter().map(|x| (x.0.to_owned(), x.1.to_owned())))
        }
        self
    }

    /// Adds the arguments to every container
    pub fn add_common_entrypoint_args(&mut self, args: &[&str]) -> &mut Self {
        for container in self.containers.values_mut() {
            container
                .entrypoint_args
                .extend(args.iter().map(|x| x.to_string()))
        }
        self
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
                if let Some(mut runner) = self.container_runners.remove(*name) {
                    let _ = runner.terminate().await;
                    self.container_results
                        .insert(name.to_string(), Ok(runner.get_command_result().unwrap()));
                }
            }
        }
    }

    /// Force removes all active containers.
    pub async fn terminate_containers(&mut self) {
        for name in self.active_names() {
            if let Some(docker_id) = self.active_container_ids.remove(&name) {
                // TODO we should parse errors to differentiate whether it is
                // simply a race condition where the container finished before
                // this time, or is a proper command runner error.
                let _ = Command::new("docker rm -f", &[&docker_id])
                    .run_to_completion()
                    .await;
                if let Some(mut runner) = self.container_runners.remove(&name) {
                    let _ = runner.terminate().await;
                    self.container_results
                        .insert(name.to_string(), Ok(runner.get_command_result().unwrap()));
                }
            }
        }
    }

    /// Force removes all active containers and removes the network
    pub async fn terminate_all(&mut self) {
        self.terminate_containers().await;
        if self.network_active {
            let _ = Command::new("docker network rm", &[&self.network_name_with_uuid()])
                .run_to_completion()
                .await;
            self.network_active = false;
        }
    }

    /// Runs only the given `names`
    pub async fn run(&mut self, names: &[&str], ci_mode: bool) -> Result<()> {
        if ci_mode {
            info!(
                "`ContainerNetwork::run(ci_mode: true, ..)` with UUID {}",
                self.uuid_as_string()
            )
        }
        // relatively cheap preverification should be done first to prevent much more
        // expensive later undos
        let mut set = BTreeSet::new();
        for name in names {
            if set.contains(name) {
                return Err(Error::from(format!(
                    "ContainerNetwork::run() two containers were supplied with the same name \
                     \"{name}\""
                )))
            }
            if !self.containers.contains_key(*name) {
                return Err(Error::from(format!(
                    "ContainerNetwork::run() argument name \"{name}\" is not contained in the \
                     network"
                )))
            }
            set.insert(*name);
        }

        let debug_log = FileOptions::write2(
            &self.log_dir,
            format!("container_network_{}.log", self.network_name),
        );
        // prechecking the log directory
        debug_log
            .preacquire()
            .await
            .stack_err(|| "ContainerNetwork::run() when acquiring logs directory")?;

        let mut get_dockerfile_write_dir = false;
        for name in names {
            let container = &self.containers[*name];
            if let Some(ref path) = container.entrypoint_path {
                acquire_file_path(path).await?;
            }
            match container.dockerfile {
                Dockerfile::NameTag(_) => {
                    // adds unnecessary time to common case, just catch it at
                    // build time or else we should add a flag to do this step
                    // (which does update the image if it has new commits)
                    /*let comres = Command::new("docker pull", &[&name_tag])
                        .ci_mode(ci_mode)
                        .stdout_log(&debug_log)
                        .stderr_log(&debug_log)
                        .run_to_completion()
                        .await?;
                    comres.assert_success().stack_err(|| {
                        format!("could not pull image for `Dockerfile::Image({name_tag})`")
                    })?;*/
                }
                Dockerfile::Path(ref path) => {
                    acquire_file_path(path)
                        .await
                        .stack_err(|| "could not find dockerfile path")?;
                }
                Dockerfile::Contents(_) => get_dockerfile_write_dir = true,
            }
        }
        let mut dockerfile_write_dir = None;
        let mut dockerfile_write_file = None;
        if get_dockerfile_write_dir {
            let mut path = acquire_dir_path(self.dockerfile_write_dir.as_ref().unwrap())
                .await
                .stack_err(|| "could not find `dockerfile_write_dir` directory")?;
            dockerfile_write_dir = Some(path.to_str().stack()?.to_owned());
            path.push("__tmp.dockerfile");
            dockerfile_write_file = Some(path.to_str().unwrap().to_owned());
        }

        /*
        for name in names {
            // remove potentially previously existing container with same name
            let _ = Command::new("docker rm -f", &[name])
                // never put in CI mode or put in debug file, error on nonexistent container is
                // confusing, actual errors will be returned
                .ci_mode(false)
                .run_to_completion()
                .await?;
        }
        */

        if !self.network_active {
            // remove old network if it exists (there is no option to ignore nonexistent
            // networks, drop exit status errors and let the creation command handle any
            // higher order errors)
            /*let _ = Command::new("docker network rm", &[&self.network_name_with_uuid()])
            .ci_mode(false)
            .stdout_log(&debug_log)
            .stderr_log(&debug_log)
            .run_to_completion()
            .await;*/
            let comres = if self.is_not_internal {
                Command::new("docker network create", &[&self.network_name_with_uuid()])
                    .ci_mode(false)
                    .stdout_log(&debug_log)
                    .stderr_log(&debug_log)
                    .run_to_completion()
                    .await?
            } else {
                Command::new("docker network create --internal", &[
                    &self.network_name_with_uuid()
                ])
                .ci_mode(false)
                .stdout_log(&debug_log)
                .stderr_log(&debug_log)
                .run_to_completion()
                .await?
            };
            // TODO we can get the network id
            comres.assert_success().stack()?;
            self.network_active = true;
        }

        // run all the creation first so that everything is pulled and prepared
        for name in names {
            let container = &self.containers[*name];

            let bin_path = if let Some(ref path) = container.entrypoint_path {
                Some(acquire_file_path(path).await?)
            } else {
                None
            };
            let bin_s = bin_path.map(|p| p.file_name().unwrap().to_str().unwrap().to_owned());
            let bin_s = bin_s.as_ref();

            // baseline args
            let network_name = self.network_name_with_uuid();
            let hostname = self.hostname_with_uuid(name).unwrap();
            let full_name = self.container_name_with_uuid(name).unwrap();
            let mut args = vec![
                "create",
                "--rm",
                "--network",
                &network_name,
                "--hostname",
                &hostname,
                "--name",
                &full_name,
            ];

            let mut tmp = vec![];
            for var in &container.environment_vars {
                tmp.push(format!("{}={}", var.0, var.1));
            }
            for tmp in &tmp {
                args.push("-e");
                args.push(tmp);
            }

            // volumes
            let mut volumes = container.volumes.clone();
            // include the needed binary
            if let Some(bin_s) = bin_s {
                volumes.push((
                    container.entrypoint_path.as_ref().unwrap().to_owned(),
                    format!("/usr/bin/{bin_s}"),
                ));
            }
            let mut combined_volumes = vec![];
            for volume in &volumes {
                let path = acquire_path(&volume.0)
                    .await
                    .stack_err(|| "could not locate local part of volume argument")?;
                combined_volumes.push(format!("{}:{}", path.to_str().stack()?, volume.1));
            }
            for volume in &combined_volumes {
                args.push("--volume");
                args.push(volume);
            }

            // other creation args
            for create_arg in &container.create_args {
                args.push(create_arg);
            }

            match container.dockerfile {
                Dockerfile::NameTag(ref name_tag) => {
                    // tag using `name_tag`
                    args.push("-t");
                    args.push(name_tag);
                }
                Dockerfile::Path(ref path) => {
                    // tag
                    args.push("-t");
                    args.push(&full_name);

                    let mut dockerfile = acquire_file_path(path).await?;
                    // yes we do need to do this because of the weird way docker build works
                    let dockerfile_full = dockerfile.to_str().unwrap().to_owned();
                    let mut build_args =
                        vec!["build", "-t", &full_name, "--file", &dockerfile_full];
                    dockerfile.pop();
                    let dockerfile_dir = dockerfile.to_str().unwrap().to_owned();
                    let mut tmp = vec![];
                    for arg in &container.build_args {
                        tmp.push(arg);
                    }
                    for s in &tmp {
                        build_args.push(s);
                    }
                    build_args.push(&dockerfile_dir);
                    Command::new("docker", &build_args)
                        .ci_mode(ci_mode)
                        .stdout_log(&debug_log)
                        .stderr_log(&debug_log)
                        .run_to_completion()
                        .await?
                        .assert_success()
                        .stack_err(|| format!("Failed when using the dockerfile at {path}"))?;
                }
                Dockerfile::Contents(ref contents) => {
                    // tag
                    args.push("-t");
                    args.push(&full_name);

                    FileOptions::write_str(dockerfile_write_file.as_ref().unwrap(), contents)
                        .await?;
                    let mut build_args = vec![
                        "build",
                        "-t",
                        &full_name,
                        "--file",
                        dockerfile_write_file.as_ref().unwrap(),
                    ];
                    let mut tmp = vec![];
                    for arg in &container.build_args {
                        tmp.push(arg);
                    }
                    for s in &tmp {
                        build_args.push(s);
                    }
                    build_args.push(dockerfile_write_dir.as_ref().unwrap());
                    Command::new("docker", &build_args)
                        .ci_mode(ci_mode)
                        .stdout_log(&debug_log)
                        .stderr_log(&debug_log)
                        .run_to_completion()
                        .await?
                        .assert_success()
                        .stack_err(|| {
                            format!(
                                "The Dockerfile::Contents written to \
                                 \"__tmp.dockerfile\":\n{contents}\n"
                            )
                        })?;
                }
            }

            // the binary
            if let Some(bin_s) = bin_s.as_ref() {
                args.push(bin_s);
            }
            // entrypoint args
            let mut tmp = vec![];
            for arg in &container.entrypoint_args {
                tmp.push(arg.to_owned());
            }
            for s in &tmp {
                args.push(s);
            }
            let command = Command::new("docker", &args)
                .ci_mode(ci_mode && matches!(container.dockerfile, Dockerfile::NameTag(_)))
                .stdout_log(&debug_log)
                .stderr_log(&debug_log);
            if ci_mode {
                info!("`Container` creation command: {command:#?}");
            }
            match command.run_to_completion().await {
                Ok(output) => {
                    match output.assert_success() {
                        Ok(_) => {
                            let mut docker_id = output.stdout;
                            // remove trailing '\n'
                            docker_id.pop();
                            match String::from_utf8(docker_id) {
                                Ok(docker_id) => {
                                    self.active_container_ids
                                        .insert(name.to_string(), docker_id);
                                }
                                Err(e) => return Err(Error::from(e)),
                            }
                        }
                        Err(e) => {
                            self.terminate_all().await;
                            return Err(e)
                        }
                    }
                }
                Err(e) => {
                    self.terminate_all().await;
                    return e.stack_err(|| "{self:?}.run()")
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
        self.run(&v, ci_mode).await.stack()
    }

    /// Looks through the results and includes the last "Error: Error { stack:
    /// [" or " panicked at " parts of stdouts. Omits stacks that have
    /// "ProbablyNotRootCauseError".
    fn error_compilation(&mut self) -> Result<()> {
        let not_root_cause = "ProbablyNotRootCauseError";
        let error_stack = "Error { stack: [";
        let panicked_at = " panicked at ";
        let mut res = Error::empty();
        for (name, result) in &self.container_results {
            match result {
                Ok(comres) => {
                    if !comres.successful() {
                        let mut encountered = false;
                        let stdout = comres.stdout_as_utf8_lossy();
                        if let Some(start) = stdout.rfind(error_stack) {
                            if !stdout.contains(not_root_cause) {
                                encountered = true;
                                res = res.add_kind_locationless(format!(
                                    "Error stack from container \"{name}\":\n{}\n",
                                    &stdout[start..]
                                ));
                            }
                        }

                        if let Some(i) = stdout.rfind(panicked_at) {
                            if let Some(i) = stdout[0..i].rfind("thread") {
                                encountered = true;
                                res = res.add_kind_locationless(format!(
                                    "Panic message from container \"{name}\":\n{}\n",
                                    &stdout[i..]
                                ));
                            }
                        }

                        if (!encountered) && (!comres.successful_or_terminated()) {
                            res = res.add_kind_locationless(format!(
                                "Error: Container \"{name}\" was unsuccessful but does not seem \
                                 to have an error stack or panic message\n"
                            ));
                        }
                    }
                }
                Err(e) => {
                    res = res.add_kind_locationless(format!(
                        "Command runner level error from container {name}:\n{e:?}\n"
                    ));
                }
            }
        }
        Err(res)
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
                            // we put in some extra delay so that the log file writers have some
                            // extra time to finish
                            sleep(Duration::from_millis(300)).await;
                            self.terminate_all().await;
                        }
                        return Err(Error::from(format!(
                            "ContainerNetwork::wait_with_timeout() timeout waiting for container \
                             names {names:?} to complete"
                        )))
                    }
                } else {
                    sleep(Duration::from_millis(256)).await;
                }
            }

            let name = &names[i];
            let runner = self.container_runners.get_mut(name).stack_err(|| {
                "ContainerNetwork::wait_with_timeout -> name \"{name}\" not found in the network"
            })?;
            match runner.wait_with_timeout(Duration::ZERO).await {
                Ok(()) => {
                    self.active_container_ids.remove(name).unwrap();
                    let runner = self.container_runners.remove(name).unwrap();
                    let first = runner.get_command_result().unwrap();
                    let err = !first.successful();
                    self.container_results.insert(name.clone(), Ok(first));
                    if terminate_on_failure && err {
                        sleep(Duration::from_millis(300)).await;
                        self.terminate_all().await;
                        return self.error_compilation().stack_err(|| {
                            "ContainerNetwork::wait_with_timeout(terminate_on_failure: true) error \
                             compilation (check logs for more):\n"
                        })
                    }
                    names.remove(i);
                }
                Err(e) => {
                    if !e.is_timeout() {
                        self.active_container_ids.remove(name).unwrap();
                        let mut runner = self.container_runners.remove(name).unwrap();
                        runner.terminate().await?;
                        self.container_results.insert(name.clone(), Err(e));
                        if terminate_on_failure {
                            sleep(Duration::from_millis(300)).await;
                            self.terminate_all().await;
                        }
                        return self.error_compilation().stack_err(|| {
                            "ContainerNetwork::wait_with_timeout(terminate_on_failure: true) error \
                             compilation (check logs for more):\n"
                        })
                    }
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// Runs [ContainerNetwork::wait_with_timeout] on all active containers.
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
