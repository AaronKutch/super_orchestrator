use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    mem,
    net::IpAddr,
    sync::atomic::Ordering,
    time::Duration,
};

use stacked_errors::{Error, Result, StackableErr};
use tokio::time::{sleep, Instant};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    docker::{Container, Dockerfile},
    docker_helpers::wait_get_ip_addr,
    Command, CommandResult, CommandRunner, FileOptions, CTRLC_ISSUED,
};

#[derive(Debug, Default)]
#[allow(clippy::large_enum_variant)]
enum RunState {
    #[default]
    PreActive,
    Active(CommandRunner),
    PostActive(Result<CommandResult>),
}

#[derive(Debug)]
struct ContainerState {
    container: Container,
    run_state: RunState,
    // NOTE: logically, only the `Active` state should have actual containers that should be
    // removed before program exit, but in the run function there is a loop that first creates all
    // containers before starting them. If an error occurs in between, the created containers with
    // associated IDs need to be removed. The drop and terminate functions only deals with this
    // variable and assume that panicking is happening or the state is cleaned up before giving
    // back to a user.
    active_container_id: Option<String>,
    already_tried_drop: bool,
}

impl Drop for ContainerState {
    fn drop(&mut self) {
        if self.already_tried_drop {
            // avoid recursive panics if something goes wrong in the `Command`
            return
        }
        self.already_tried_drop = true;
        if let Some(id) = self.active_container_id.take() {
            let _ = std::process::Command::new("docker")
                .arg("rm")
                .arg("-f")
                .arg(id)
                .output();
        }
    }
}

impl ContainerState {
    // returns if there was an error from a `CommandRunner`.
    #[must_use]
    pub async fn terminate(&mut self) -> bool {
        if let Some(id) = self.active_container_id.take() {
            let _ = Command::new("docker rm -f")
                .arg(id)
                .run_to_completion()
                .await;
        }
        let state = mem::take(&mut self.run_state);
        match state {
            RunState::PreActive => false,
            RunState::Active(mut runner) => match runner.terminate().await {
                Ok(()) => {
                    if let Some(comres) = runner.take_command_result() {
                        let err = !comres.successful();
                        self.run_state = RunState::PostActive(Ok(comres));
                        err
                    } else {
                        self.run_state = RunState::PostActive(Err(Error::from_kind_locationless(
                            "ContainerNetwork -> when terminating a `CommandRunner` attached to a \
                             container, did not find a command result for some reason",
                        )));
                        true
                    }
                }
                Err(e) => {
                    self.run_state = RunState::PostActive(Err(e.add_kind_locationless(
                        "ContainerNetwork -> when terminating a `CommandRunner` attached to a \
                         container, encountered an unexpected error",
                    )));
                    true
                }
            },
            RunState::PostActive(x) => {
                self.run_state = RunState::PostActive(x);
                false
            }
        }
    }

    pub fn new(container: Container) -> Self {
        Self {
            container,
            run_state: RunState::PreActive,
            active_container_id: None,
            already_tried_drop: false,
        }
    }

    pub fn container(&self) -> &Container {
        &self.container
    }

    pub fn container_mut(&mut self) -> &mut Container {
        &mut self.container
    }

    pub fn is_active(&self) -> bool {
        matches!(self.run_state, RunState::Active(_))
    }
}

/// A controlled network of containers.
///
/// This allows for much more control than docker-compose does. Every
/// `ContainerNetwork` generates a new UUID for enabling multiple
/// `ContainerNetworks` from the same base to run concurrently. By default these
/// are not applied, but it is recommended to enable them if possible (which may
/// require passing around the UUID parameter for hostnames).
///
/// # Note
///
/// If a CTRL-C/sigterm signal is sent while containers are running, and
/// [ctrlc_init](crate::ctrlc_init) has not been set up, the containers may
/// continue to run in the background and will have to be manually stopped. If
/// the handlers are set, then one of the runners will trigger an error or a
/// check for `CTRLC_ISSUED` will terminate all.
#[derive(Debug)]
pub struct ContainerNetwork {
    uuid: Uuid,
    network_name: String,
    /// Arguments passed to `docker network create` when any container is first
    /// run
    pub network_args: Vec<String>,
    set: BTreeMap<String, ContainerState>,
    dockerfile_write_dir: Option<String>,
    log_dir: String,
    network_active: bool,
    /// If build commands should be `debug`
    pub debug_build: bool,
    /// If create commands should be `debug`
    pub debug_create: bool,
    /// If extra debug output should be enabled
    pub debug_extra: bool,
    already_tried_drop: bool,
}

impl Drop for ContainerNetwork {
    fn drop(&mut self) {
        // in case something panics recursively
        if self.already_tried_drop {
            return
        }
        self.already_tried_drop = true;
        // here we are only concerned with logically active containers in a non
        // panicking situation, the `Drop` impl on each `ContainerState` handles the
        // rest if necessary
        let removed_set = mem::take(&mut self.set);
        for state in removed_set.values() {
            // we purposely order in this way to avoid calling `panicking` in the
            // normal case
            if state.is_active() && (!std::thread::panicking()) {
                warn!(
                    "A `ContainerNetwork` was dropped without all active containers being \
                     properly terminated"
                );
                break
            }
        }
        for (_, state) in removed_set {
            drop(state);
        }
        // all the containers should be removed now
        if self.network_active {
            let _ = std::process::Command::new("docker")
                .arg("network")
                .arg("rm")
                .arg(self.network_name())
                .output();
        }
    }
}

impl ContainerNetwork {
    /// Creates a new `ContainerNetwork`.
    ///
    /// `network_name` sets the name of the docker network that containers will
    /// be attached to, `dockerfile_write_dir` is the directory in
    /// which ".tmp.dockerfile" files can be written if `Dockerfile::Contents`
    /// is used (unless the `dockerfile_write_file`s are explicitly set),
    /// and `log_dir` is where ".log" log files will be written.
    ///
    /// The docker network is only actually created the first time a container
    /// is run.
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
    pub fn new<S0, S1>(network_name: S0, dockerfile_write_dir: Option<&str>, log_dir: S1) -> Self
    where
        S0: AsRef<str>,
        S1: AsRef<str>,
    {
        Self {
            uuid: Uuid::new_v4(),
            network_name: network_name.as_ref().to_owned(),
            network_args: vec![],
            set: BTreeMap::new(),
            dockerfile_write_dir: dockerfile_write_dir.map(|s| s.to_owned()),
            log_dir: log_dir.as_ref().to_owned(),
            network_active: false,
            debug_build: false,
            debug_create: false,
            debug_extra: false,
            already_tried_drop: false,
        }
    }

    /// Same as [ContainerNetwork::new], but it adds a UUID suffix to the
    /// `network_name``
    pub fn new_with_uuid<S0, S1>(
        network_name: S0,
        dockerfile_write_dir: Option<&str>,
        log_dir: S1,
    ) -> Self
    where
        S0: AsRef<str>,
        S1: AsRef<str>,
    {
        let mut cn = Self::new(network_name, dockerfile_write_dir, log_dir);
        cn.network_name = format!("{}_{}", cn.network_name, cn.uuid);
        cn
    }

    /// Adds arguments to be passed to `docker network create` (which will be
    /// run once any container is started)
    pub fn add_network_args<I, S>(&mut self, network_args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.network_args
            .extend(network_args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Returns the common UUID
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// Returns the common UUID as a string
    pub fn uuid_as_string(&self) -> String {
        self.uuid.to_string()
    }

    /// Returns the network name
    pub fn network_name(&self) -> &str {
        &self.network_name
    }

    /// Adds the container to the inactive set
    pub fn add_container(&mut self, container: Container) -> Result<&mut Self> {
        if self.dockerfile_write_dir.is_none()
            && matches!(container.dockerfile, Dockerfile::Contents(_))
        {
            return Err(Error::from_kind_locationless(
                "ContainerNetwork::add_container -> a container is built with \
                 `Dockerfile::Contents`, but `dockerfile_write_dir` is unset",
            ))
        }
        match self.set.entry(container.name.clone()) {
            Entry::Vacant(v) => {
                v.insert(ContainerState::new(container));
            }
            Entry::Occupied(_) => {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::add_container -> two containers were supplied with the \
                     same name \"{}\"",
                    container.name
                )))
            }
        }
        Ok(self)
    }

    /// Removes the container with `name` from the network, force terminating it
    /// if it is currently active. Returns `Ok(None)` if the container was never
    /// activated. Should return a `CommandResult` if the container was normally
    /// terminated. Returns an error if `name` could not be found.
    pub async fn remove_container<S>(&mut self, name: S) -> Result<Option<CommandResult>>
    where
        S: AsRef<str>,
    {
        let name = name.as_ref();
        self.terminate([name]).await;
        if let Some(mut state) = self.set.remove(name) {
            match mem::take(&mut state.run_state) {
                RunState::PostActive(Ok(comres)) => Ok(Some(comres)),
                _ => Ok(None),
            }
        } else {
            Err(Error::from(format!(
                "ContainerNetwork::remove_name -> could not find name \"{name}\" in the network"
            )))
        }
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
        for state in self.set.values_mut() {
            state
                .container_mut()
                .volumes
                .extend(volumes.iter().cloned());
        }
        self
    }

    /// Adds the arguments to every container currently in the network
    pub fn add_common_entrypoint_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        for state in self.set.values_mut() {
            state
                .container_mut()
                .entrypoint_args
                .extend(args.iter().cloned())
        }
        self
    }

    /// Get a map of active container names to ids
    pub fn get_active_container_ids(&self) -> BTreeMap<String, String> {
        let mut v = BTreeMap::new();
        for (name, state) in &self.set {
            if state.is_active() {
                v.insert(name.to_string(), state.active_container_id.clone().unwrap());
            }
        }
        v
    }

    /// Get the names of all active containers
    pub fn active_names(&self) -> Vec<String> {
        let mut v = vec![];
        for (name, state) in &self.set {
            if state.is_active() {
                v.push(name.to_string());
            }
        }
        v
    }

    /// Get the names of all inactive containers (both containers that have not
    /// been run before, and containers that were terminated)
    pub fn inactive_names(&self) -> Vec<String> {
        let mut v = vec![];
        for (name, state) in &self.set {
            if !state.is_active() {
                v.push(name.to_string());
            }
        }
        v
    }

    /// Force removes any active containers found with the given names
    pub async fn terminate<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for name in names {
            let name = name.as_ref();
            if let Some(state) = self.set.get_mut(name) {
                let _ = state.terminate().await;
            }
        }
    }

    /// Force removes all active containers, but does not remove the docker
    /// network
    pub async fn terminate_containers(&mut self) {
        for state in self.set.values_mut() {
            let _ = state.terminate().await;
        }
    }

    // don't make public because we would have to make decisions around containers
    // that still exist
    /// Removes the docker network
    async fn terminate_network(&mut self) {
        if self.network_active {
            let _ = Command::new("docker network rm")
                .arg(self.network_name())
                .run_to_completion()
                .await;
            self.network_active = false;
        }
    }

    /// Force removes all active containers and removes the network. The
    /// `ContainerNetwork` can always be safely dropped if this is the last
    /// function called on it. The network is recreated if any containers are
    /// run again.
    pub async fn terminate_all(&mut self) {
        self.terminate_containers().await;
        self.terminate_network().await;
    }

    /// Runs only the given `names`. This prechecks as much as it can before
    /// creating any containers. If an error happens in the middle of creating
    /// and starting the containers, any of the `names` that had been created
    /// are terminated before the function returns.
    pub async fn run<I, S>(&mut self, names: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // avoid polymorphizing such a large function
        self.run_internal(
            &names
                .into_iter()
                .map(|s| s.as_ref().to_owned())
                .collect::<Vec<String>>(),
        )
        .await
    }

    async fn run_internal(&mut self, names: &[String]) -> Result<()> {
        let debug_extra = self.debug_extra;
        if self.debug_build || self.debug_create || self.debug_extra {
            debug!("ContainerNetwork::run with UUID {}", self.uuid_as_string());
        }
        // relatively cheap preverification should be done first to prevent much more
        // expensive later undos
        let mut set = BTreeSet::new();
        for name in names {
            if set.contains(name) {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::run -> two containers were supplied with the same name \
                     \"{name}\""
                )))
            }
            if let Some(state) = self.set.get(name) {
                if state.is_active() {
                    return Err(Error::from_kind_locationless(format!(
                        "ContainerNetwork::run -> name \"{name}\" is already an active container"
                    )))
                }
            } else {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::run -> argument name \"{name}\" is not contained in the \
                     network"
                )))
            }
            set.insert(name.to_string());
        }

        if debug_extra {
            debug!("prechecking");
        }

        let log_file = FileOptions::write2(
            &self.log_dir,
            format!("container_network_{}.log", self.network_name()),
        );
        log_file.preacquire().await.stack_err_locationless(|| {
            "ContainerNetwork::run -> could not acquire logs directory"
        })?;

        for name in names {
            let container = &mut self.set.get_mut(name).unwrap().container;
            match container.dockerfile {
                Dockerfile::NameTag(_) => (),
                Dockerfile::Path(_) => (),
                Dockerfile::Contents(_) => {
                    if let Some(file_path) = &container.dockerfile_write_file {
                        FileOptions::write(file_path)
                            .preacquire()
                            .await
                            .stack_err_locationless(|| {
                                format!(
                                    "ContainerNetwork::run -> could not acquire the explicitly \
                                     set `dockerfile_write_file` on container with name \"{name}\""
                                )
                            })?;
                    } else if let Some(dir) = &self.dockerfile_write_dir {
                        let path = FileOptions::write2(dir, format!("{name}.tmp.dockerfile"))
                            .preacquire()
                            .await
                            .stack_err_locationless(|| {
                                "ContainerNetwork::run -> could not acquire the \
                                 `dockerfile_write_dir`"
                            })?;
                        container.dockerfile_write_file = Some(
                            path.to_str()
                                .stack_err_locationless(|| {
                                    "ContainerNetwork::run -> could not acquire the \
                                     `dockerfile_write_dir` as a UTF8 path"
                                })?
                                .to_owned(),
                        );
                    } else {
                        return Err(Error::from_kind_locationless(format!(
                            "ContainerNetwork::run -> the `dockerfile_write_dir` on the \
                             `ContainerNetwork` or the `dockerfile_write_file` on container with \
                             name \"{name}\" needs to be set"
                        )));
                    }
                }
            }
        }

        for name in names {
            let container = &mut self.set.get_mut(name).unwrap().container;
            container.precheck().await.stack_err_locationless(|| {
                format!("ContainerNetwork::run -> when prechecking container {container:#?}")
            })?;
        }

        if debug_extra {
            debug!("building");
        }

        // TODO eventually move this capability to the struct level so that it handles
        // many stage `ContainerNetwork::run`

        // The trick with the build stage is that we want to build as little as we have
        // to. The build stage only uses  `dockerfile` and `build_args` with respect to
        // determinism, so here we order them and reduce redundancies.
        let mut build_to_image = BTreeMap::<(Dockerfile, Vec<String>), (String, String)>::new();
        let uuid = self.uuid();
        for name in names.iter() {
            let container = &mut self.set.get_mut(name).unwrap().container;
            if container.build_tag.is_none() {
                match build_to_image
                    .entry((container.dockerfile.clone(), container.build_args.clone()))
                {
                    Entry::Vacant(v) => {
                        let image = format!("super_orchestrator_{name}_{uuid}");
                        container.build_tag = Some(image.clone());
                        v.insert((name.clone(), image.clone()));
                    }
                    Entry::Occupied(o) => {
                        // set the `build_tag` to an already planned image
                        container.build_tag = Some(o.get().1.clone());
                    }
                }
            } // else it was explicitly set or built in a previous run
        }

        // run all the build commands that we actually need
        for (name, _) in build_to_image.values() {
            let state = self.set.get_mut(name).unwrap();
            state
                .container()
                .build(self.debug_build)
                .await
                .stack_err_locationless(|| {
                    format!("ContainerNetwork::run when building the container for name \"{name}\"")
                })?;
        }

        if debug_extra {
            debug!("creating");
        }

        if !self.network_active {
            // remove old network if it exists (there is no option to ignore nonexistent
            // networks, drop exit status errors and let the creation command handle any
            // higher order errors)
            /*let _ = Command::new("docker network rm", &[&self.network_name()])
            .debug(false)
            .stdout_log(&debug_log)
            .stderr_log(&debug_log)
            .run_to_completion()
            .await;*/
            let comres = Command::new("docker network create --internal")
                .args(self.network_args.iter())
                .arg(self.network_name())
                .run_to_completion()
                .await
                .stack_err_locationless(|| {
                    "ContainerNetwork::run -> when running network creation command"
                })?;
            // TODO we can get the network id
            comres
                .assert_success()
                .stack_err_locationless(|| "ContainerNetwork::run -> failed to create network")?;
            self.network_active = true;
        }

        // run all of the creation first so that everything is pulled and prepared
        let network_name = &self.network_name;
        for (i, name) in names.iter().enumerate() {
            let state = self.set.get_mut(name).unwrap();
            match state
                .container()
                .create(network_name, None, self.debug_create)
                .await
                .stack_err_locationless(|| {
                    format!("ContainerNetwork::run when creating the container for name \"{name}\"")
                }) {
                Ok(docker_id) => {
                    state.active_container_id = Some(docker_id);
                }
                Err(e) => {
                    // need to fix all the containers in the intermediate state
                    for name in &names[..i] {
                        let _ = self.set.get_mut(name).unwrap().terminate().await;
                    }
                    e.stack_err_locationless(|| {
                        format!(
                            "ContainerNetwork::run when creating the container for name \"{name}\""
                        )
                    })?;
                }
            }
        }

        if debug_extra {
            debug!("starting");
        }

        // start containers
        for name in names {
            let state = self.set.get_mut(name).unwrap();
            let (stdout_log, stderr_log) = if state.container.log {
                (
                    Some(state.container.stdout_log.clone().unwrap_or_else(|| {
                        FileOptions::write2(&self.log_dir, format!("{}_stdout.log", name))
                    })),
                    Some(state.container.stderr_log.clone().unwrap_or_else(|| {
                        FileOptions::write2(&self.log_dir, format!("{}_stderr.log", name))
                    })),
                )
            } else {
                (None, None)
            };
            match state
                .container()
                .start(
                    state.active_container_id.as_ref().unwrap(),
                    stdout_log.as_ref(),
                    stderr_log.as_ref(),
                )
                .await
                .stack_err_locationless(|| {
                    format!("ContainerNetwork::run when starting the container for name \"{name}\"")
                }) {
                Ok(runner) => {
                    state.run_state = RunState::Active(runner);
                }
                Err(e) => {
                    for name in names.iter() {
                        let _ = self.set.get_mut(name).unwrap().terminate().await;
                    }
                    return Err(e)
                }
            }
        }

        if debug_extra {
            debug!("started");
        }

        Ok(())
    }

    /// [ContainerNetwork::run] on all inactive containers in the network. Note
    /// that terminated containers that weren't removed are recreated.
    pub async fn run_all(&mut self) -> Result<()> {
        let names = self.inactive_names();
        let mut v: Vec<&str> = vec![];
        for name in &names {
            v.push(name);
        }
        self.run(&v)
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
        for (name, state) in self.set.iter() {
            // TODO not sure if we should have a generation counter to track different sets
            // of `wait_*` failures, for now we will just always use all unsuccessful
            // `PostActive` containers
            if let RunState::PostActive(ref result) = state.run_state {
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
                                    "Error: Container \"{name}\" was unsuccessful but does not \
                                     seem to have an error stack or panic message\n"
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        res = res.add_kind_locationless(format!(
                            "Error: The internal handling of Container \"{name}\" produced this \
                             error:\n {e:?}"
                        ));
                    }
                }
            }
        }
        Err(res)
    }

    /// Waits for the containers with `names` to all complete, or returns if
    /// `duration` timeout is exceeded.
    ///
    /// If `terminate_on_failure`, then if there is a timeout or any
    /// container from `names` has an error, then the whole network will be
    /// terminated.
    ///
    /// By default, if any container stops normally but with an unsuccessful
    /// return value (not just the `names` but any container in the network),
    /// the `wait_with_timeout` function will return or terminate everything if
    /// `terminate_on_failure`. This can be changed by setting the
    /// `allow_unsuccessful` flag on the desired `Container`s.
    ///
    /// Note that if a CTRL-C/sigterm signal is sent and
    /// [ctrlc_init](crate::ctrlc_init) has been run, then an internal
    /// [CTRLC_ISSUED] check will trigger
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
        for name in names.iter() {
            if let Some(state) = self.set.get(name) {
                if !state.is_active() {
                    return Err(Error::from(format!(
                        "ContainerNetwork::wait_with_timeout -> name \"{name}\" is already \
                         inactive"
                    )));
                }
            } else {
                return Err(Error::from(format!(
                    "ContainerNetwork::wait_with_timeout -> name \"{name}\" not found in the \
                     network"
                )));
            }
        }
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
            let state = self.set.get_mut(name).unwrap();
            if let RunState::Active(ref mut runner) = state.run_state {
                match runner.wait_with_timeout(Duration::ZERO).await {
                    Ok(()) => {
                        // avoid double terminate
                        let err = {
                            if let Some(comres) = runner.take_command_result() {
                                let err = !comres.successful();
                                state.run_state = RunState::PostActive(Ok(comres));
                                err
                            } else {
                                state.run_state =
                                    RunState::PostActive(Err(Error::from_kind_locationless(
                                        "ContainerNetwork::wait_with_timeout -> when runner was \
                                         done, did not find a command result for some reason",
                                    )));
                                true
                            }
                        };
                        if terminate_on_failure && err && (!state.container.allow_unsuccessful) {
                            // give some time for other containers to react, they will be sending
                            // ProbablyNotRootCause errors and other things
                            sleep(Duration::from_millis(300)).await;
                            self.terminate_all().await;
                            return self.error_compilation().stack_err_locationless(|| {
                                "ContainerNetwork::wait_with_timeout error compilation (check logs \
                                 for more):\n"
                            })
                        }
                        names.remove(i);
                    }
                    Err(e) => {
                        if !e.is_timeout() {
                            let _ = runner.terminate().await;
                            if terminate_on_failure {
                                // give some time like in the earlier case
                                sleep(Duration::from_millis(300)).await;
                                self.terminate_all().await;
                            }
                            return self
                                .error_compilation()
                                .stack_err_locationless(|| {
                                    "ContainerNetwork::wait_with_timeout encountered OS-level \
                                     `CommandRunner` error"
                                })
                                .stack_err_locationless(|| {
                                    "ContainerNetwork::wait_with_timeout error compilation (check \
                                     logs for more):\n"
                                })
                        }
                        i += 1;
                    }
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
        let state = self.set.get(name).stack_err_locationless(|| {
            format!(
                "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                 name: {name}) -> could not find name in container network"
            )
        })?;
        let id = state
            .active_container_id
            .as_ref()
            .stack_err_locationless(|| {
                format!(
                    "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                     name: {name}) -> found container, but it was not active"
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

    /// Sets whether the `Container::build` commands should produce debug output
    pub fn debug_build(&mut self, debug_build: bool) -> &mut Self {
        self.debug_build = debug_build;
        self
    }

    /// Sets whether the `Container::create` commands should produce debug
    /// output
    pub fn debug_create(&mut self, debug_create: bool) -> &mut Self {
        self.debug_create = debug_create;
        self
    }

    /// Sets other debug info
    pub fn debug_extra(&mut self, debug_extra: bool) -> &mut Self {
        self.debug_extra = debug_extra;
        self
    }

    /// Sets all debug flags at once
    pub fn debug_all(&mut self, debug_all: bool) -> &mut Self {
        self.debug_build(debug_all);
        self.debug_create(debug_all);
        self.debug_extra(debug_all)
    }
}
