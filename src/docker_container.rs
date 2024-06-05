use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use stacked_errors::{Error, Result, StackableErr};
use tracing::info;

use crate::{
    acquire_file_path, acquire_path, docker::ContainerNetwork, next_terminal_color, Command,
    CommandResult, CommandRunner, FileOptions,
};

// No `OsString`s or `PathBufs` for these structs, it introduces too many issues
// (e.g. the commands get sent to docker and I don't know exactly what
// normalization it performs). Besides, this should be as cross platform as
// possible.

/// Ways of using a dockerfile for building a container
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Container running information, put this into a `ContainerNetwork`
///
/// # Note
///
/// Weird things happen if volumes to the same container overlap, e.g. if
/// the directory used for logs is added as a volume, and a volume to another
/// path contained within the directory is also added as a volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    /// The name of the container as referenced by
    pub name: String,
    /// The name that the container is tagged with, note that the "name:tag"
    /// dockerhub image argument would go in [Dockerfile::NameTag]
    pub tag_name: String,
    /// Hostname of the URL that could access the container (the container can
    /// alternatively be accessed by an ip address). Usually, this should be the
    /// same as `name`.
    pub host_name: String,
    /// The dockerfile
    pub dockerfile: Dockerfile,
    /// Any flags and args passed to to `docker build`
    pub build_args: Vec<String>,
    /// Any flags and args passed to to `docker create`
    pub create_args: Vec<String>,
    /// Passed as `--volume string0:string1` to the create args, but these have
    /// the advantage of being canonicalized and prechecked
    pub volumes: Vec<(String, String)>,
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
    /// Set by default, this tells the `ContainerNetwork` to forward
    /// stdout/stderr from `docker start`
    pub debug: bool,
    /// Set by default, this tells the `ContainerNetwork` to copy stdout/stderr
    /// to log files in the log directory
    pub log: bool,
    /// If `log` is set, then this will override the file that the
    /// `ContainerNetwork` chooses
    pub stdout_log: Option<FileOptions>,
    /// If `log` is set, then this will override the file that the
    /// `ContainerNetwork` chooses
    pub stderr_log: Option<FileOptions>,
}

impl Container {
    /// Creates the information needed to describe a `Container`. `name` is used
    /// for the `name`, `tag_name`, and `hostname`.
    pub fn new<S>(name: S, dockerfile: Dockerfile) -> Self
    where
        S: AsRef<str>,
    {
        let name = name.as_ref();
        Self {
            name: name.to_owned(),
            tag_name: name.to_owned(),
            host_name: name.to_owned(),
            dockerfile,
            build_args: vec![],
            create_args: vec![],
            volumes: vec![],
            workdir: None,
            environment_vars: vec![],
            entrypoint_file: None,
            entrypoint_args: vec![],
            debug: true,
            log: true,
            stdout_log: None,
            stderr_log: None,
        }
    }

    /// This is used in the entrypoint pattern where an externally compiled
    /// binary is used as the entrypoint for the container. This adds a volume
    /// from `entrypoint_binary` to "/{binary_file_name}", sets
    /// `entrypoint_file` to that, and also adds the
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
            .stack_err_locationless(|| {
                "Container::external_entrypoint could not acquire the external entrypoint binary"
            })?;
        let binary_file_name = binary_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let entrypoint_file = format!("/{binary_file_name}");
        self.entrypoint_file = Some(entrypoint_file.clone());
        self.volumes.push((
            binary_path.as_os_str().to_str().unwrap().to_owned(),
            entrypoint_file,
        ));
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

    /// Adds a volume
    pub fn volume(mut self, key: impl AsRef<str>, val: impl AsRef<str>) -> Self {
        self.volumes
            .push((key.as_ref().to_owned(), val.as_ref().to_owned()));
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
    pub fn workdir<S: AsRef<str>>(mut self, workdir: S) -> Self {
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

    /// Runs this container by itself in a default `ContainerNetwork` with
    /// "super_orchestrator_{uuid}" as the network name, waiting for completion
    /// with a timeout.
    pub async fn run(
        self,
        dockerfile_write_dir: Option<&str>,
        timeout: Duration,
        log_dir: &str,
        debug: bool,
    ) -> Result<CommandResult> {
        // TODO UUID for this for instance
        let mut cn = ContainerNetwork::new("super_orchestrator", dockerfile_write_dir, log_dir);
        let name = self.name.clone();
        cn.add_container(self).stack_err_locationless(|| {
            "Container::run when trying to create a `ContainerNetwork`"
        })?;
        cn.run_all(debug)
            .await
            .stack_err_locationless(|| "Container::run when trying to run a `ContainerNetwork`")?;
        cn.wait_with_timeout_all(true, timeout)
            .await
            .stack_err_locationless(|| "Container::run when waiting on its `ContainerNetwork`")?;
        cn.terminate_all().await;
        cn.remove_container(name)
            .await
            .unwrap()
            .stack_err_locationless(|| {
                "Container::run could not get `CommandResult` because of some internal bug or error"
            })
    }

    /// This function is intended to be indirectly used by most users through
    /// [Container::run] or [ContainerNetwork]. Uses `docker build` and
    /// `docker create` to create a container corresponding to `self`, returning
    /// the container ID. `dockerfile_write_file` should be an acquired and
    /// UTF8-converted string to the temporary dockerfile.
    pub async fn create(
        &self,
        network_name: &str,
        dockerfile_write_file: &Option<PathBuf>,
        debug: bool,
        log_file: Option<&FileOptions>,
    ) -> Result<String> {
        match self.dockerfile {
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
                    "Container::create -> could not acquire the path in a `Dockerfile::Path`"
                })?;
            }
            Dockerfile::Contents(_) => {
                if dockerfile_write_file.is_none() {
                    return Err(Error::from(
                        "Container::create -> `Dockerfile::Contents` requires a \
                         `dockerfile_write_dir`, but none was provided",
                    ));
                }
            }
        }

        let tag_name = &self.tag_name;
        let hostname = &self.host_name;
        let mut args = vec![
            "create",
            "--rm",
            "--network",
            &network_name,
            "--hostname",
            &hostname,
            "--name",
            &tag_name,
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
        for volume in &self.volumes {
            let path = acquire_path(&volume.0).await.stack_err_locationless(|| {
                "Container::create -> could not acquire_path to local part of volume argument"
            })?;
            combined_volumes.push(format!(
                "{}:{}",
                path.to_str()
                    .stack_err_locationless(|| "Container::create -> path was not UTF-8")?,
                volume.1
            ));
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
                args.push("-t");
                args.push(name_tag);
            }
            Dockerfile::Path(ref path) => {
                // tag
                args.push("-t");
                args.push(tag_name);

                let mut dockerfile = acquire_file_path(path).await?;
                // yes we do need to do this because of the weird way docker build works
                let dockerfile_full = dockerfile.to_str().unwrap().to_owned();
                let mut build_args = vec!["build", "-t", &tag_name, "--file", &dockerfile_full];
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
                Command::new("docker")
                    .args(build_args)
                    .log(log_file)
                    .run_to_completion()
                    .await?
                    .assert_success()
                    .stack_err_locationless(|| {
                        format!("Container::create -> when using the dockerfile at {path:?}")
                    })?;
            }
            Dockerfile::Contents(ref contents) => {
                // we prechecked this
                let dockerfile_write_file = dockerfile_write_file.as_ref().unwrap();
                // tag
                args.push("-t");
                args.push(tag_name);

                FileOptions::write_str(&dockerfile_write_file, contents).await?;
                let mut build_args: Vec<&str> = vec![
                    "build",
                    "-t",
                    &tag_name,
                    "--file",
                    &dockerfile_write_file.to_str().stack()?,
                ];
                let mut tmp: Vec<&str> = vec![];
                for arg in &self.build_args {
                    tmp.push(arg);
                }
                for s in &tmp {
                    build_args.push(s);
                }
                let mut dockerfile_write_dir = dockerfile_write_file.to_owned();
                dockerfile_write_dir.pop();
                build_args.push(dockerfile_write_dir.to_str().stack()?);
                Command::new("docker")
                    .args(build_args)
                    .log(log_file)
                    .run_to_completion()
                    .await?
                    .assert_success()
                    .stack_err_locationless(|| {
                        format!(
                            "Container::create -> when using the `Dockerfile::Contents` written \
                             to \"{dockerfile_write_file:?}\":\n{contents}\n"
                        )
                    })?;
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
        let command = Command::new("docker").args(args).log(log_file);
        if debug {
            // purposely use this format
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
                            Ok(docker_id) => Ok(docker_id),
                            Err(e) => Err(Error::from_kind_locationless(e)),
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => {
                Err(e).stack_err_locationless(|| "Container::create -> when creating the container")
            }
        }
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
        let terminal_color = if true {
            next_terminal_color()
        } else {
            owo_colors::AnsiColors::Default
        };
        let mut command = Command::new("docker start --attach").arg(container_id);
        if self.debug {
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
        if self.log {
            command = command.stdout_log(stdout_log).stderr_log(stderr_log);
        }
        let runner = command
            .run()
            .await
            .stack_err_locationless(|| "Container::start")?;
        Ok(runner)
    }
}
