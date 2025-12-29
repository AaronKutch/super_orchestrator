use std::{fs::read_to_string, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use stacked_errors::{bail_locationless, Error, Result, StackableErr};
use tracing::debug;
use uuid::Uuid;

use crate::{
    acquire_file_path, acquire_path, cli_docker::ContainerNetwork, next_terminal_color, Command,
    CommandResult, CommandRunner, FileOptions,
};

// No `OsString`s or `PathBufs` for these structs, it introduces too many issues
// (e.g. the commands get sent to docker and I don't know exactly what
// normalization it performs). Besides, this should be as cross platform as
// possible.

/// Ways of using a dockerfile for building a container
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Dockerfile {
    /// Builds using an image in the format "name:tag" such as "fedora:41" or
    /// "alpine:3.21" (running will call something such as `docker pull
    /// name:tag`). Docker will first try to fetch from the local registry, in
    /// which case there might not be a tag, and just the name of some image
    /// should be used.
    NameTag(String),
    /// Builds from a dockerfile on a path (e.x.
    /// "./tests/dockerfiles/example.dockerfile")
    Path(String),
    /// Builds from contents that are written to "{name}.tmp.dockerfile" in a
    /// directory determined by the `ContainerNetwork`. Note that resources used
    /// by this dockerfile may need to be in the same directory.
    Contents(String),
}

impl Dockerfile {
    /// Returns `Self::NameTag` with the argument
    pub fn name_tag(name_and_tag: impl AsRef<str>) -> Self {
        Self::NameTag(name_and_tag.as_ref().to_owned())
    }

    /// Returns `Self::Path` with the argument
    pub fn path(path_to_dockerfile: impl AsRef<str>) -> Self {
        Self::Path(path_to_dockerfile.as_ref().to_owned())
    }

    /// Returns `Self::Contents` with the argument
    pub fn contents(contents_of_dockerfile: impl AsRef<str>) -> Self {
        Self::Contents(contents_of_dockerfile.as_ref().to_owned())
    }

    pub fn add_build_steps<I, S>(mut self, build_steps: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let contents = match self {
            Dockerfile::NameTag(name_tag) => format!("FROM {}", name_tag),
            Dockerfile::Path(file_path) => read_to_string(file_path).stack()?,
            Dockerfile::Contents(contents) => contents,
        };
        let contents = build_steps
            .into_iter()
            .fold(contents, |out, step| out + "\n" + step.as_ref());
        Ok(Dockerfile::Contents(contents))
    }
}

/// Configuration for running a container.
///
/// The `docker run` command can be split into separate `docker build`, `docker
/// create`, and `docker start` commands (which are what `super_orchestrator`
/// calls instead for precision and reuse). `docker build` and `docker create`
/// take different subsets of options from `docker run`. If you want to pass
/// arguments that you would when manually running `docker run`, e.x.
/// `docker run --no-cache -p 127.0.0.1:5432:5432`, then you need to look at
/// `docker build --help` and `docker create --help` to figure out which
/// subcommand the options belong to. In this case "--no-cache" belongs to
/// `docker build` and "-p" belongs to `docker create`. This means that you
/// would call
/// `Container::new(...).build_args(["--no-cache"]).create_args(["-p",
/// "127.0.0.1:5432:5432"])` to create container configuration that would run
/// with the right options.
///
/// Remember that there are debug options you can set to print out the exact
/// commands that are being called internally.
///
/// # Note
///
/// Broken behavior often results on the docker side if volumes to the same
/// container overlap, e.g. if the directory used for logs is added as a volume,
/// and a volume to another path contained within the same directory is also
/// added as a volume.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Container {
    /// The name of the container as it will be referenced by in a
    /// `DockerNetwork`
    pub name: String,
    /// The name that the container is actually named with, note that the
    /// "name:tag" dockerhub image argument would go in
    /// [Dockerfile::NameTag] or in the `build_tag`
    pub container_name: String,
    /// Hostname of the URL that could access the container (the container can
    /// alternatively be accessed by an ip address via
    /// [wait_get_ip_addr](crate::cli_docker::wait_get_ip_addr)). Usually,
    /// this should be the same as `name`.
    pub host_name: String,
    /// The dockerfile
    pub dockerfile: Dockerfile,
    /// Any flags and args passed to to `docker build`
    pub build_args: Vec<String>,
    /// The tag used for images, this is set automatically by `ContainerNetwork`
    /// but can be set to override the image it would automatically build
    pub build_tag: Option<String>,
    /// Any flags and args passed to to `docker create`
    pub create_args: Vec<String>,
    /// Passed as `--volume string0:string1` to the create args, but these have
    /// the advantage of being canonicalized and prechecked
    pub volumes: Vec<(String, String)>,
    pub copied_contents: Vec<(String, String)>,
    /// Working directory inside the container
    pub workdir: Option<String>,
    /// Environment variable mappings passed to docker
    pub environment_vars: Vec<(String, String)>,
    /// When set, this indicates that the container should run an entrypoint
    /// using this path to a binary in the container
    pub entrypoint_file: Option<String>,
    /// Passed in as ["arg1", "arg2", ...] with the bracket and quotations being
    /// added
    pub entrypoint_args: Vec<String>,
    /// Changes what some functions allow to fail when running the container
    pub allow_unsuccessful: bool,
    /// Set by default, this tells the `ContainerNetwork` to forward
    /// stdout/stderr from `docker start`
    pub debug: bool,
    /// Unset by default, this tells the `ContainerNetwork` to copy
    /// stdout/stderr to log files in the log directory
    pub log: bool,
    /// If `log` is set, then this will override the file that the
    /// `ContainerNetwork` chooses
    pub stdout_log: Option<FileOptions>,
    /// If `log` is set, then this will override the file that the
    /// `ContainerNetwork` chooses
    pub stderr_log: Option<FileOptions>,
    /// This can be explicitly set to override the default temporary file that
    /// `ContainerNetwork` uses
    pub dockerfile_write_file: Option<String>,
}

fn apply_debug(command: Command, name: &str, debug: bool) -> Command {
    if debug {
        let terminal_color = next_terminal_color();
        command
            .debug(true)
            .stdout_debug_line_prefix(Some(
                owo_colors::OwoColorize::color(&format!("{name}  | "), terminal_color).to_string(),
            ))
            .stderr_debug_line_prefix(Some(
                owo_colors::OwoColorize::color(&format!("{name} E| "), terminal_color).to_string(),
            ))
    } else {
        command
    }
}

impl Container {
    /// Creates the information needed to describe a `Container`. `name` is used
    /// for the `name`, `container_name`, and `hostname`.
    pub fn new<S>(name: S, dockerfile: Dockerfile) -> Self
    where
        S: AsRef<str>,
    {
        let name = name.as_ref();
        Self {
            name: name.to_owned(),
            build_tag: None,
            container_name: name.to_owned(),
            host_name: name.to_owned(),
            dockerfile,
            build_args: vec![],
            create_args: vec![],
            volumes: vec![],
            copied_contents: vec![],
            workdir: None,
            environment_vars: vec![],
            entrypoint_file: None,
            entrypoint_args: vec![],
            allow_unsuccessful: false,
            debug: true,
            log: false,
            stdout_log: None,
            stderr_log: None,
            dockerfile_write_file: None,
        }
    }

    /// This is used in the entrypoint pattern where an externally compiled
    /// binary is used as the entrypoint for the container. This adds a volume
    /// from `entrypoint_binary` to "/{binary_file_name}_{uuid}" (the UUID is
    /// for preventing accidental collisions with things in the file system),
    /// sets `entrypoint_file` to that, and also adds the
    /// `entrypoint_args`. Returns an error if the binary file path cannot be
    /// acquired.
    pub async fn external_entrypoint<I, S>(
        mut self,
        entrypoint_binary: impl AsRef<str>,
        entrypoint_args: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let binary_path = acquire_file_path(entrypoint_binary.as_ref())
            .await
            .stack_err_locationless(
                "Container::external_entrypoint could not acquire the external entrypoint binary",
            )?;
        let binary_file_name = binary_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let uuid = Uuid::new_v4();
        let entrypoint_file = format!("/{binary_file_name}_{uuid}");
        self.entrypoint_file = Some(entrypoint_file.clone());
        self.volumes.push((
            binary_path.as_os_str().to_str().unwrap().to_owned(),
            entrypoint_file,
        ));
        self.entrypoint_args
            .extend(entrypoint_args.into_iter().map(|s| s.as_ref().to_string()));
        Ok(self)
    }

    pub async fn copy_entrypoint<I, S>(
        mut self,
        entrypoint_relative_host_path: impl AsRef<str>,
        entrypoint_args: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let binary_path = acquire_file_path(entrypoint_relative_host_path.as_ref())
            .await
            .stack_err_locationless(
                "Container::copy_entrypoint could not acquire the external entrypoint binary",
            )?;
        let binary_file_name = binary_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let uuid = Uuid::new_v4();
        let entrypoint_file = format!("/{binary_file_name}_{uuid}");
        self.entrypoint_file = Some(entrypoint_file.clone());
        self.dockerfile = self
            .dockerfile
            .add_build_steps([
                format!(
                    "COPY {} {}",
                    entrypoint_relative_host_path.as_ref(),
                    entrypoint_file
                ),
                format!("RUN chmod +x {}", entrypoint_file),
            ])
            .stack()?;

        self.entrypoint_args
            .extend(entrypoint_args.into_iter().map(|s| s.as_ref().to_string()));
        Ok(self)
    }

    /// Sets `entrypoint_file` and adds to `entrypoint_args`
    pub fn entrypoint<I, S>(mut self, entrypoint_file: impl AsRef<str>, entrypoint_args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.entrypoint_file = Some(entrypoint_file.as_ref().to_owned());
        self.entrypoint_args
            .extend(entrypoint_args.into_iter().map(|s| s.as_ref().to_string()));
        self
    }

    /// Adds an entrypoint argument
    pub fn entrypoint_arg(mut self, entrypoint_arg: impl AsRef<str>) -> Self {
        self.entrypoint_args
            .push(entrypoint_arg.as_ref().to_string());
        self
    }

    /// Adds a volume to map a local path to a path in the container
    pub fn volume(mut self, local: impl AsRef<str>, container: impl AsRef<str>) -> Self {
        self.volumes
            .push((local.as_ref().to_owned(), container.as_ref().to_owned()));
        self
    }

    /// Adds multiple volumes
    pub fn volumes<I, K, V>(mut self, volumes: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.volumes.extend(
            volumes
                .into_iter()
                .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string())),
        );
        self
    }

    /// Add arguments to be passed to `docker build`
    pub fn build_args<I, S>(mut self, build_args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.build_args
            .extend(build_args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Add arguments to be passed to `docker create`
    pub fn create_args<I, S>(mut self, create_args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.create_args
            .extend(create_args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Adds environment vars to be passed
    pub fn environment_vars<I, K, V>(mut self, environment_vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.environment_vars.extend(
            environment_vars
                .into_iter()
                .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string())),
        );
        self
    }

    /// Sets the working directory inside the container
    pub fn workdir(mut self, workdir: impl AsRef<str>) -> Self {
        self.workdir = Some(workdir.as_ref().to_string());
        self
    }

    /// Add arguments to be passed to the entrypoint
    pub fn entrypoint_args<I, S>(mut self, entrypoint_args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.entrypoint_args
            .extend(entrypoint_args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Sets whether a container is allowed to have an unsuccesful output
    pub fn allow_unsuccessful(mut self, allow_unsuccessful: bool) -> Self {
        self.allow_unsuccessful = allow_unsuccessful;
        self
    }

    /// Sets whether container stdout/stderr should be forwarded
    pub fn debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Sets whether container stdout/stderr should be written to log files
    pub fn log(mut self, log: bool) -> Self {
        self.log = log;
        self
    }

    /// Sets the `dockerfile_write_file` used for the `Dockerfile::Contents`
    /// option explicitly
    pub fn dockerfile_write_file(mut self, file_path: Option<String>) -> Self {
        self.dockerfile_write_file = file_path;
        self
    }

    /// Runs this container by itself in a default `ContainerNetwork` with
    /// "super_orchestrator_{uuid}" as the network name, waiting for completion
    /// with a timeout. Setting `debug` is equivalent to setting `debug_build`
    /// and `debug_create` on a `ContainerNetwork`. Unconditionally sets
    /// `allow_unsuccessful`, so the `CommandResult` has to be checked if there
    /// was an unsuccessful error return status from within the container
    /// itself.
    pub async fn run(
        self,
        dockerfile_write_dir: Option<&str>,
        timeout: Duration,
        log_dir: &str,
        debug: bool,
    ) -> Result<CommandResult> {
        let mut cn =
            ContainerNetwork::new_with_uuid("super_orchestrator", dockerfile_write_dir, log_dir);
        cn.debug_build(debug).debug_create(debug);
        let name = self.name.clone();
        cn.add_container(self.allow_unsuccessful(true))
            .stack_err_locationless("Container::run when trying to create a `ContainerNetwork`")?;

        // in order to get unsuccesful `CommandResult`s, we do not terminate on failure
        // and need to remember to `terminate_all` before returning other kinds of
        // errors
        cn.run_all()
            .await
            .stack_err_locationless("Container::run when trying to run a `ContainerNetwork`")?;
        cn.wait_with_timeout_all(true, timeout)
            .await
            .stack_err_locationless("Container::run when waiting on its `ContainerNetwork`")?;
        cn.terminate_all().await;

        cn.remove_container(name)
            .await
            .unwrap()
            .stack_err_locationless(
                "Container::run could not get `CommandResult` because of some internal bug or \
                 error",
            )
    }

    /// Prechecks several things needed to successfully run `self`, and
    /// normalizes paths like the local parts of volumes. This and subsequent
    /// steps are automatically handled in [Container::run] or
    /// [ContainerNetwork::run]. Checks `dockerfile_write_dir.is_some()` if
    /// `Dockerfile::Contents` but does not preacquire
    /// `dockerfile_write_dir`.
    pub async fn precheck(&mut self) -> Result<()> {
        match self.dockerfile {
            Dockerfile::NameTag(_) => (),
            Dockerfile::Path(ref path) => {
                acquire_file_path(path).await.stack_err_locationless(
                    "Container::precheck -> could not acquire the path in a `Dockerfile::Path`",
                )?;
            }
            Dockerfile::Contents(_) => {
                if self.dockerfile_write_file.is_none() {
                    bail_locationless!(
                        "Container::precheck -> `Dockerfile::Contents` requires a \
                         `dockerfile_write_dir`, but none was provided",
                    );
                }
            }
        }
        for (local_content, _) in &mut self.copied_contents {
            let path = acquire_path(&local_content).await.stack_err_locationless(
                "Container::precheck -> could not acquire_path to local part of volume argument",
            )?;
            path.to_str()
                .stack_err_locationless("Container::precheck -> path was not UTF-8")?
                .clone_into(local_content);
        }
        for (local_volume, _) in &mut self.volumes {
            let path = acquire_path(&local_volume).await.stack_err_locationless(
                "Container::precheck -> could not acquire_path to local part of volume argument",
            )?;
            path.to_str()
                .stack_err_locationless("Container::precheck -> path was not UTF-8")?
                .clone_into(local_volume);
        }

        Ok(())
    }

    /// Runs `docker build` to create a container corresponding to `self`
    /// (preferably after [Container::precheck] is run). `build_tag` needs to be
    /// set unless `Dockerfile::NameTag` was used.
    pub async fn build(&self, debug_build: bool) -> Result<()> {
        // NOTE: `ContainerNetwork::run_internal` assumes that builds are uniquely
        // determined from `dockerfile` and `build_args`.
        let build_tag = &self
            .build_tag
            .as_ref()
            .stack_err_locationless("Container::build -> the `build_tag` needs to be set")?;
        match self.dockerfile {
            Dockerfile::NameTag(ref _name_tag) => {
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
                let mut dockerfile = acquire_file_path(path).await?;
                // yes we do need to do this because of the weird way docker build works
                let dockerfile_full = dockerfile.to_str().unwrap().to_owned();
                let mut build_args = vec!["build", "-t", build_tag, "--file", &dockerfile_full];
                dockerfile.pop();
                let dockerfile_dir = dockerfile.to_str().unwrap().to_owned();
                let mut tmp = vec![];
                for arg in &self.build_args {
                    tmp.push(arg);
                }
                for s in &tmp {
                    build_args.push(s);
                }
                build_args.push(&dockerfile_dir);
                let command = apply_debug(
                    Command::new("docker").args(build_args),
                    &self.name,
                    debug_build,
                );
                if debug_build {
                    debug!("Container::build command: {command:#?}");
                }
                command
                    .run_to_completion()
                    .await?
                    .assert_success()
                    .stack_err_with_locationless(|| {
                        format!("Container::build -> when using the dockerfile at {path:?}")
                    })?;
            }
            Dockerfile::Contents(ref contents) => {
                let dockerfile_write_file = self.dockerfile_write_file.as_ref().stack()?;
                FileOptions::write_str(&dockerfile_write_file, contents).await?;
                let mut build_args: Vec<&str> =
                    vec!["build", "-t", build_tag, "--file", &dockerfile_write_file];
                let mut tmp: Vec<&str> = vec![];
                for arg in &self.build_args {
                    tmp.push(arg);
                }
                for s in &tmp {
                    build_args.push(s);
                }
                let mut dockerfile_write_dir = PathBuf::from(dockerfile_write_file.to_owned());
                dockerfile_write_dir.pop();
                build_args.push(dockerfile_write_dir.to_str().unwrap());
                let command = apply_debug(
                    Command::new("docker").args(build_args),
                    &self.name,
                    debug_build,
                );
                if debug_build {
                    debug!("Container::build command: {command:#?}");
                }
                command
                    .run_to_completion()
                    .await?
                    .assert_success()
                    .stack_err_with_locationless(|| {
                        format!(
                            "Container::build -> when using the `Dockerfile::Contents` written to \
                             \"{dockerfile_write_file:?}\":\n{contents}\n"
                        )
                    })?;
            }
        }

        Ok(())
    }

    /// Runs `docker create` to create a container corresponding to `self`
    /// (preferably after running [Container::build]). `build_tag` needs to be
    /// set unless `Dockerfile::NameTag` was used.
    pub async fn create(
        &self,
        network_name: &str,
        log_file: Option<&FileOptions>,
        debug_create: bool,
    ) -> Result<String> {
        let container_name = &self.container_name;
        let hostname = &self.host_name;
        let mut args = vec![
            "create",
            "--rm",
            "--network",
            &network_name,
            "--hostname",
            &hostname,
            "--name",
            &container_name,
        ];

        if let Some(workdir) = self.workdir.as_ref() {
            args.push("-w");
            args.push(workdir)
        }

        let mut tmp = vec![];
        for var in &self.environment_vars {
            tmp.push(format!("{}={}", var.0, var.1));
        }
        for tmp in &tmp {
            args.push("-e");
            args.push(tmp);
        }

        // volumes
        let mut combined_volumes = vec![];
        for (local_volume, virtual_volume) in &self.volumes {
            // assumes normalization from `precheck_and_normalize`
            combined_volumes.push(format!("{local_volume}:{virtual_volume}",));
        }
        for volume in &combined_volumes {
            args.push("--volume");
            args.push(volume);
        }

        // other creation args
        for create_arg in &self.create_args {
            args.push(create_arg);
        }

        match self.dockerfile {
            Dockerfile::NameTag(ref name_tag) => {
                // tag using `name_tag`
                args.push(name_tag);
            }
            Dockerfile::Path(_) | Dockerfile::Contents(_) => {
                // use the tag of the build image
                args.push(
                    self.build_tag.as_ref().stack_err_locationless(
                        "Container::create -> `build_tag` needs to be set",
                    )?,
                );
            }
        }

        // the binary
        if let Some(s) = self.entrypoint_file.as_ref() {
            args.push(s);
        }
        // entrypoint args
        let mut tmp = vec![];
        for arg in &self.entrypoint_args {
            tmp.push(arg.to_owned());
        }
        for s in &tmp {
            args.push(s);
        }
        let command =
            apply_debug(Command::new("docker").args(args), &self.name, debug_create).log(log_file);
        if debug_create {
            debug!("Container::create command: {command:#?}");
        }
        let res = match command.run_to_completion().await {
            Ok(output) => {
                match output.assert_success() {
                    Ok(_) => {
                        let mut docker_id = output.stdout;
                        // remove trailing '\n'
                        docker_id.pop();
                        match String::from_utf8(docker_id) {
                            Ok(docker_id) => Ok(docker_id),
                            Err(e) => Err(Error::from_err_locationless(e)),
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => {
                Err(e).stack_err_locationless("Container::create -> when creating the container")
            }
        };

        let Ok(docker_id) = res else { return res };
        for (local_path, virtual_path) in &self.copied_contents {
            //so the docker syntax is source <container>:dest
            let command = apply_debug(
                Command::new("docker").args(vec![
                    "cp",
                    local_path.as_str(),
                    &format!("{}:{}", docker_id, virtual_path.as_str()),
                ]),
                &self.name,
                debug_create,
            )
            .log(log_file);
            match command.run_to_completion().await {
                Ok(_) => {}
                Err(e) => {
                    return Err(e).stack_err_locationless(
                        "Container::copy -> when copying contents to container after creation",
                    )
                }
            };
        }
        Ok(docker_id)
    }

    /// Runs `docker start` on a `container_id` (preferably from
    /// [Container::create]), setting up a `CommandRunner` based on `self`.
    pub async fn start(
        &self,
        container_id: &str,
        stdout_log: Option<&FileOptions>,
        stderr_log: Option<&FileOptions>,
    ) -> Result<CommandRunner> {
        let name = &self.name;
        let mut command = apply_debug(
            Command::new("docker start --attach").arg(container_id),
            name,
            self.debug,
        );
        if self.log {
            command = command.stdout_log(stdout_log).stderr_log(stderr_log);
        }
        let runner = command
            .run()
            .await
            .stack_err_locationless("Container::start")?;
        Ok(runner)
    }
}
