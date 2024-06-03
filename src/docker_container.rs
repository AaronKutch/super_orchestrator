use std::time::Duration;

use serde::{Deserialize, Serialize};
use stacked_errors::{Result, StackableErr};

use crate::{acquire_file_path, docker::ContainerNetwork, CommandResult};

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
    /// The name of the container, note the "name:tag" docker argument would go
    /// in [Dockerfile::NameTag]
    pub name: String,
    /// Hostname of the URL that could access the container (the container can
    /// alternatively be accessed by an ip address). Usually, this should be the
    /// same as `name``.
    pub host_name: String,
    /// If true, `host_name` is used directly without appending a UUID
    pub no_uuid_for_host_name: bool,
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
}

impl Container {
    /// Creates the information needed to describe a `Container`. `name` is used
    /// for both the `name` and `hostname`.
    pub fn new(name: &str, dockerfile: Dockerfile) -> Self {
        Self {
            name: name.to_owned(),
            host_name: name.to_owned(),
            no_uuid_for_host_name: false,
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

    /// Turns of the default behavior of attaching the UUID to the hostname
    pub fn no_uuid_for_host_name(mut self) -> Self {
        self.no_uuid_for_host_name = true;
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
    /// "super_orchestrator" as the network name, waiting for completion with a
    /// timeout.
    pub async fn run(
        self,
        dockerfile_write_dir: Option<&str>,
        timeout: Duration,
        log_dir: &str,
        debug: bool,
    ) -> Result<CommandResult> {
        let mut cn = ContainerNetwork::new(
            "super_orchestrator",
            vec![self],
            dockerfile_write_dir,
            true,
            log_dir,
        )
        .stack_err_locationless(|| "Container::run when trying to create a `ContainerNetwork`")?;
        cn.run_all(debug)
            .await
            .stack_err_locationless(|| "Container::run when trying to run a `ContainerNetwork`")?;
        cn.wait_with_timeout_all(true, timeout)
            .await
            .stack_err_locationless(|| "Container::run when waiting on its `ContainerNetwork`")?;
        cn.terminate_all().await;
        Ok(cn.container_results.pop_first().unwrap().1.unwrap())
    }
}
