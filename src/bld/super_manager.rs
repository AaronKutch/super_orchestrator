mod impls;

use std::{collections::HashMap, net::IpAddr, path::PathBuf, pin::Pin};

pub use bollard::{
    container::{AttachContainerResults, LogOutput},
    errors::Error as BollardError,
    image::BuildImageOptions,
    secret::ContainerState,
};

use super::super_docker_file::{SuperDockerFile, SuperImage};

/// Define port mapping like -p <host_ip>:<host_port>:<container_port>. Usually
/// this shouldn't be used for integration testing because all container should
/// already be accessible.
#[derive(Debug, Clone)]
pub struct PortBind {
    container_port: u16,
    host_port: Option<u16>,
    host_ip: Option<IpAddr>,
}

/// Define `docker create` arguments with integration testing in mind. You can
/// construct a container using the [SuperNetwork] struct.
#[derive(Debug, Clone, Default)]
pub struct SuperContainerOptions {
    pub name: String,
    pub user: Option<String>,
    pub cmd: Vec<String>,
    pub env_vars: Vec<String>,
    pub volumes: Vec<(String, String)>,
    pub cap_adds: Vec<String>,
    pub sysctls: HashMap<String, String>,
    pub priviledged: bool,
    pub port_bindings: Vec<PortBind>,
    /// Wether the network should wait for the container to shutdown during a
    /// [SuperNetwork::wait_important] call
    pub important: bool,
    /// When defined, overwrites the default set in [SuperNetwork].
    ///
    /// When log_outs is activated the network will use `docker attach` to read
    /// stdout and stderr from container and log to stderr
    pub log_outs: Option<bool>,
}

pub type DockerStdin = Pin<Box<dyn tokio::io::AsyncWrite + Send>>;
pub type DockerOutput =
    Pin<Box<dyn futures::stream::Stream<Item = Result<LogOutput, BollardError>> + Send>>;
pub type OutputHook = Box<dyn Fn(&Result<LogOutput, BollardError>) -> stacked_errors::Result<()>>;

struct LiveContainer {
    image: SuperImage,
    container_opts: SuperContainerOptions,
    network_opts: SuperNetworkContainerOptions,
    should_be_started: bool,
    stdin: Option<DockerStdin>,
    output_dir: Option<PathBuf>,
}

pub const SUPER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME: &str = "SUPER_NETWORK_OUTPUT_DIR";
pub fn get_network_output_dir() -> Option<String> {
    std::env::var(SUPER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME).ok()
}

#[derive(Debug)]
pub struct SuperNetwork {
    // might be good for debug
    #[allow(dead_code)]
    network_id: String,
    opts: SuperCreateNetworkOptions,
    containers: HashMap<String, LiveContainer>,
}

/// If any field is None, it'll be equivalent to passing no argument to `docker
/// create` command.
#[derive(Debug, Clone, Default)]
pub struct SuperNetworkContainerOptions {
    pub hostname: Option<String>,
    pub mac_address: Option<String>,
}

#[derive(Debug)]
pub enum AddContainerOptions {
    /// Use an already specified image to create the container
    Container {
        image: SuperImage,
    },
    DockerFile {
        docker_file: SuperDockerFile,
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
    /// Directory for dealing with outputs. Use a temporary directory or an
    /// ignored directory in a repository. This won't mount the output dir,
    /// it'll create other directories and mount them. The instruction for
    /// outputs will be like this:
    ///
    /// `VOLUME <output_dir>/<container_name> /super_out`.
    ///
    /// This is necessary if using the test_entrypoint option in the test_opts
    ///
    /// This also adds env var SUPER_NETWORK_OUTPUT_DIR to the process. Query
    /// using the env var to ensure compatibility.
    pub output_dir: String,
    /// Write all captured output to a log file
    pub save_logs: bool,
}
