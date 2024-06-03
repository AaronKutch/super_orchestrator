use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    sync::atomic::Ordering,
    time::Duration,
};

use stacked_errors::{Error, Result, StackableErr};
use tokio::time::{sleep, Instant};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    acquire_dir_path, acquire_file_path, acquire_path,
    docker::{Container, Dockerfile},
    docker_helpers::wait_get_ip_addr,
    next_terminal_color, Command, CommandResult, CommandRunner, FileOptions, CTRLC_ISSUED,
};

/// A complete network of one or more containers, a more programmable
/// alternative to `docker-compose`
///
/// # Note
///
/// If a CTRL-C/sigterm signal is sent while containers are running, and
/// [ctrlc_init](crate::ctrlc_init) or some other handler has not been set up,
/// the containers may continue to run in the background and will have to be
/// manually stopped. If the handlers are set, then one of the runners will
/// trigger an error or a check for `CTRLC_ISSUED` will terminate all.
///
/// When running multiple containers with networking, there is an
/// [issue](<https://github.com/moby/libnetwork/issues/2647>)
/// on some platforms that means you may have to set `is_not_internal` to `true`
/// even if networking is only done between containers within the network.
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
    /// This function generates a UUID used for enabling multiple
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
                return Err(Error::from_kind_locationless(
                    "ContainerNetwork::new() a container is built with `Dockerfile::Contents`, \
                     but `dockerfile_write_dir` is unset",
                ))
            }
            if map.contains_key(&container.name) {
                return Err(Error::from_kind_locationless(format!(
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

    /// Returns the common UUID
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// Returns the common UUID as a string
    pub fn uuid_as_string(&self) -> String {
        self.uuid.to_string()
    }

    /// Returns the full network name
    pub fn network_name_with_uuid(&self) -> String {
        format!("{}_{}", self.network_name, self.uuid)
    }

    /// Returns an error if the container with the name could not be found
    pub fn container_name_with_uuid(&self, container_name: &str) -> Result<String> {
        if let Some(container) = self.containers.get(container_name) {
            Ok(format!("{}_{}", container.name, self.uuid))
        } else {
            Err(Error::from_kind_locationless(format!(
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
            Err(Error::from_kind_locationless(format!(
                "hostname_with_uuid({container_name}): could not find container with given name"
            )))
        }
    }

    /// Adds the container to the inactive set
    pub fn add_container(&mut self, container: Container) -> Result<&mut Self> {
        if self.dockerfile_write_dir.is_none()
            && matches!(container.dockerfile, Dockerfile::Contents(_))
        {
            return Err(Error::from_kind_locationless(
                "ContainerNetwork::new() a container is built with `Dockerfile::Contents`, but \
                 `dockerfile_write_dir` is unset",
            ))
        }
        if self.containers.contains_key(&container.name) {
            return Err(Error::from_kind_locationless(format!(
                "ContainerNetwork::new() two containers were supplied with the same name \"{}\"",
                container.name
            )))
        }
        self.containers.insert(container.name.clone(), container);
        Ok(self)
    }

    /// Adds the volumes to every container currently in the network
    pub fn add_common_volumes<I, K, V>(&mut self, volumes: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let volumes: Vec<(String, String)> = volumes
            .into_iter()
            .map(|x| (x.0.as_ref().to_string(), x.1.as_ref().to_string()))
            .collect();
        for container in self.containers.values_mut() {
            container.volumes.extend(volumes.iter().cloned())
        }
        self
    }

    /// Adds the arguments to every container
    pub fn add_common_entrypoint_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        for container in self.containers.values_mut() {
            container.entrypoint_args.extend(args.iter().cloned())
        }
        self
    }

    /// Get a map of active container names to ids
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
                let _ = Command::new("docker rm -f")
                    .arg(docker_id)
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
                let _ = Command::new("docker rm -f")
                    .arg(docker_id)
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
            let _ = Command::new("docker network rm")
                .arg(self.network_name_with_uuid())
                .run_to_completion()
                .await;
            self.network_active = false;
        }
    }

    /// Runs only the given `names`
    pub async fn run(&mut self, names: &[&str], debug: bool) -> Result<()> {
        if debug {
            info!(
                "`ContainerNetwork::run(debug: true, ..)` with UUID {}",
                self.uuid_as_string()
            )
        }
        // relatively cheap preverification should be done first to prevent much more
        // expensive later undos
        let mut set = BTreeSet::new();
        for name in names {
            if set.contains(name) {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::run() two containers were supplied with the same name \
                     \"{name}\""
                )))
            }
            if !self.containers.contains_key(*name) {
                return Err(Error::from_kind_locationless(format!(
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
        debug_log.preacquire().await.stack_err_locationless(|| {
            "ContainerNetwork::run() -> could not acquire logs directory"
        })?;

        let mut get_dockerfile_write_dir = false;
        for name in names {
            let container = &self.containers[*name];
            match container.dockerfile {
                Dockerfile::NameTag(_) => {
                    // adds unnecessary time to common case, just catch it at
                    // build time or else we should add a flag to do this step
                    // (which does update the image if it has new commits)
                    /*let comres = Command::new("docker pull", &[&name_tag])
                        .debug(debug)
                        .stdout_log(&debug_log)
                        .stderr_log(&debug_log)
                        .run_to_completion()
                        .await?;
                    comres.assert_success().stack_err(|| {
                        format!("could not pull image for `Dockerfile::Image({name_tag})`")
                    })?;*/
                }
                Dockerfile::Path(ref path) => {
                    acquire_file_path(path).await.stack_err_locationless(|| {
                        "ContainerNetwork::run -> could not acquire the path in a \
                         `Dockerfile::Path`"
                    })?;
                }
                Dockerfile::Contents(_) => get_dockerfile_write_dir = true,
            }
        }
        let mut dockerfile_write_dir = None;
        let mut dockerfile_write_file = None;
        if get_dockerfile_write_dir {
            let mut path = acquire_dir_path(self.dockerfile_write_dir.as_ref().unwrap())
                .await
                .stack_err_locationless(|| {
                    "ContainerNetwork::run -> could not acquire the `dockerfile_write_dir` \
                     directory"
                })?;
            dockerfile_write_dir =
                Some(path.to_str().stack_err(|| "path was not UTF-8")?.to_owned());
            path.push("__tmp.dockerfile");
            dockerfile_write_file = Some(path.to_str().unwrap().to_owned());
        }

        /*
        for name in names {
            // remove potentially previously existing container with same name
            let _ = Command::new("docker rm -f", &[name])
                // never put in debug_log mode or put in debug file, error on
                // nonexistent container is confusing, actual errors will be returned
                .debug(false)
                .run_to_completion()
                .await?;
        }
        */

        if !self.network_active {
            // remove old network if it exists (there is no option to ignore nonexistent
            // networks, drop exit status errors and let the creation command handle any
            // higher order errors)
            /*let _ = Command::new("docker network rm", &[&self.network_name_with_uuid()])
            .debug(false)
            .stdout_log(&debug_log)
            .stderr_log(&debug_log)
            .run_to_completion()
            .await;*/
            let comres = if self.is_not_internal {
                Command::new("docker network create")
                    .arg(self.network_name_with_uuid())
                    .log(Some(&debug_log))
                    .run_to_completion()
                    .await?
            } else {
                Command::new("docker network create --internal")
                    .arg(self.network_name_with_uuid())
                    .log(Some(&debug_log))
                    .run_to_completion()
                    .await?
            };
            // TODO we can get the network id
            comres
                .assert_success()
                .stack_err_locationless(|| "ContainerNetwork::run -> failed to create network")?;
            self.network_active = true;
        }

        // run all the creation first so that everything is pulled and prepared
        for name in names {
            let container = &self.containers[*name];

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

            if let Some(workdir) = container.workdir.as_ref() {
                args.push("-w");
                args.push(workdir)
            }

            let mut tmp = vec![];
            for var in &container.environment_vars {
                tmp.push(format!("{}={}", var.0, var.1));
            }
            for tmp in &tmp {
                args.push("-e");
                args.push(tmp);
            }

            // volumes
            let mut combined_volumes = vec![];
            for volume in &container.volumes {
                let path = acquire_path(&volume.0).await.stack_err_locationless(|| {
                    "ContainerNetwork::run() -> could not acquire_path to local part of volume \
                     argument"
                })?;
                combined_volumes.push(format!(
                    "{}:{}",
                    path.to_str().stack_err(|| "path was not UTF-8")?,
                    volume.1
                ));
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
                    Command::new("docker")
                        .args(build_args)
                        .log(Some(&debug_log))
                        .run_to_completion()
                        .await?
                        .assert_success()
                        .stack_err_locationless(|| {
                            format!("Failed when using the dockerfile at {path:?}")
                        })?;
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
                    Command::new("docker")
                        .args(build_args)
                        .log(Some(&debug_log))
                        .run_to_completion()
                        .await?
                        .assert_success()
                        .stack_err_locationless(|| {
                            format!(
                                "The Dockerfile::Contents written to \
                                 \"__tmp.dockerfile\":\n{contents}\n"
                            )
                        })?;
                }
            }

            // the binary
            if let Some(s) = container.entrypoint_file.as_ref() {
                args.push(s);
            }
            // entrypoint args
            let mut tmp = vec![];
            for arg in &container.entrypoint_args {
                tmp.push(arg.to_owned());
            }
            for s in &tmp {
                args.push(s);
            }
            let command = Command::new("docker").args(args).log(Some(&debug_log));
            if debug {
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
                                Err(e) => return Err(Error::from_kind_locationless(e)),
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
                    return e.stack_err_locationless(|| "{self:?}.run()")
                }
            }
        }

        // start containers
        for name in names {
            let terminal_color = if true {
                next_terminal_color()
            } else {
                owo_colors::AnsiColors::Default
            };
            let docker_id = &self.active_container_ids[*name];
            let container = &self.containers[*name];
            let mut command = Command::new("docker start --attach").arg(docker_id);
            if container.debug {
                command = command
                    .debug(true)
                    .stdout_debug_line_prefix(Some(
                        owo_colors::OwoColorize::color(&format!("{name}  | "), terminal_color)
                            .to_string(),
                    ))
                    .stderr_debug_line_prefix(Some(
                        owo_colors::OwoColorize::color(&format!("{name} E| "), terminal_color)
                            .to_string(),
                    ));
            }
            if container.log {
                command = command
                    .stdout_log(Some(&FileOptions::write2(
                        &self.log_dir,
                        &format!("container_{}_stdout.log", name),
                    )))
                    .stderr_log(Some(&FileOptions::write2(
                        &self.log_dir,
                        &format!("container_{}_stderr.log", name),
                    )));
            }
            match command.run().await {
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

    pub async fn run_all(&mut self, debug: bool) -> Result<()> {
        let names = self.inactive_names();
        let mut v: Vec<&str> = vec![];
        for name in &names {
            v.push(name);
        }
        self.run(&v, debug)
            .await
            .stack_err_locationless(|| "ContainerNetwork::run_all")
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

    /// If `terminate_on_failure`, then if there is a timeout or any
    /// container from `names` has an error, then the whole network will be
    /// terminated.
    ///
    /// Note that if a CTRL-C/sigterm signal is sent and
    /// [ctrlc_init](crate::ctrlc_init) has been run, then either terminating
    /// runners or an internal [CTRLC_ISSUED] check will trigger
    /// [terminate_all](ContainerNetwork::terminate_all). Otherwise,
    /// containers may continue to run in the background.
    ///
    /// If called with `Duration::ZERO`, this will always complete successfully
    /// if all containers were terminated before this call.
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
            if CTRLC_ISSUED.load(Ordering::SeqCst) {
                // most of the time, a terminating runner will cause a stop before this, but
                // still check
                self.terminate_all().await;
                return Err(Error::from_kind_locationless(
                    "ContainerNetwork::wait_with_timeout terminating because of `CTRLC_ISSUED`",
                ))
            }
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
                        return Err(Error::timeout().add_kind_locationless(format!(
                            "ContainerNetwork::wait_with_timeout timeout waiting for container \
                             names {names:?} to complete"
                        )))
                    }
                } else {
                    sleep(Duration::from_millis(256)).await;
                }
            }

            let name = &names[i];
            let runner = self
                .container_runners
                .get_mut(name)
                .stack_err_locationless(|| {
                    format!(
                        "ContainerNetwork::wait_with_timeout -> name \"{name}\" not found in the \
                         network"
                    )
                })?;
            match runner.wait_with_timeout(Duration::ZERO).await {
                Ok(()) => {
                    let runner = self.container_runners.remove(name).unwrap();
                    let first = runner.get_command_result().unwrap();
                    let err = !first.successful();
                    self.container_results.insert(name.clone(), Ok(first));
                    if terminate_on_failure && err {
                        sleep(Duration::from_millis(300)).await;
                        self.terminate_all().await;
                        return self.error_compilation().stack_err_locationless(|| {
                            "ContainerNetwork::wait_with_timeout error compilation (check logs for \
                             more):\n"
                        })
                    } else {
                        // this must not happen before the `terminate_all` call or else
                        // ctrl-c conditions or others can lead the container to not be removed
                        self.active_container_ids.remove(name).unwrap();
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
                        return self.error_compilation().stack_err_locationless(|| {
                            "ContainerNetwork::wait_with_timeout error compilation (check logs for \
                             more):\n"
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

    /// Gets the IP address of an active container. There is a delay between a
    /// container starting and an IP address being assigned, which is why this
    /// has a retry mechanism.
    pub async fn wait_get_ip_addr(
        &self,
        num_retries: u64,
        delay: Duration,
        name: &str,
    ) -> Result<IpAddr> {
        let id = self
            .active_container_ids
            .get(name)
            .stack_err_locationless(|| {
                format!(
                    "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                     name: {name}) -> could not find active container with name"
                )
            })?;
        let ip = wait_get_ip_addr(num_retries, delay, id)
            .await
            .stack_err_locationless(|| {
                format!(
                    "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                     name: {name})"
                )
            })?;
        Ok(ip)
    }
}
