use std::{collections::HashMap, net::IpAddr};

use crate::{api_docker::SuperDockerFile, cli_docker::Dockerfile};

// TODO do we need this?
/// Wrapper struct for the image, call [SuperImage::get_image_id] to get the id
/// of the image as a &str or [SuperImage::into_inner] to get the underlying
/// [String].
#[derive(Debug, Clone)]
pub struct SuperImage(String);

impl SuperImage {
    pub fn new(image_id: String) -> Self {
        Self(image_id)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn get_image_id(&self) -> &str {
        &self.0
    }

    pub fn to_docker_file(&self) -> SuperDockerFile {
        SuperDockerFile::new(Dockerfile::name_tag(self.get_image_id()), None)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BootstrapOptions {
    Example,
    Bin,
    Test,
    Bench,
}

impl BootstrapOptions {
    pub fn to_flag(self) -> &'static str {
        match self {
            BootstrapOptions::Example => "--example",
            BootstrapOptions::Bin => "--bin",
            BootstrapOptions::Test => "--test",
            BootstrapOptions::Bench => "--bench",
        }
    }

    pub fn to_path_str(self) -> Option<&'static str> {
        match self {
            BootstrapOptions::Example => Some("examples"),
            BootstrapOptions::Test => Some("tests"),
            BootstrapOptions::Bench => Some("benches"),
            BootstrapOptions::Bin => None,
        }
    }
}

/// Define port mapping like -p <host_ip>:<host_port>:<container_port>. Usually
/// this shouldn't be used for integration testing because all container should
/// already be accessible.
#[derive(Debug, Clone)]
pub struct PortBind {
    container_port: u16,
    host_port: Option<u16>,
    host_ip: Option<IpAddr>,
}

impl PortBind {
    /// Results in option like <port>:<port>
    pub fn new(port: u16) -> Self {
        Self {
            container_port: port,
            host_port: Some(port),
            host_ip: None,
        }
    }

    /// Results in option like <host_port>:<container_port>
    pub fn with_host_port(mut self, port: u16) -> Self {
        self.host_port = Some(port);
        self
    }

    /// Results in option like <ip>:<host_port>:<container_port>
    pub fn with_host_ip(mut self, ip: IpAddr) -> Self {
        self.host_ip = Some(ip);
        self
    }
}

impl From<u16> for PortBind {
    fn from(port: u16) -> Self {
        Self::new(port)
    }
}

#[allow(clippy::type_complexity)] /* internal only */
pub fn port_bindings_to_bollard_args(
    pbs: &[PortBind],
) -> (
    Option<HashMap<String, HashMap<(), ()>>>,
    Option<HashMap<String, Option<Vec<bollard::secret::PortBinding>>>>,
) {
    Some(
        pbs.iter()
            .map(|pb| {
                (
                    (pb.container_port.to_string(), HashMap::new()),
                    (
                        pb.container_port.to_string(),
                        Some(vec![bollard::secret::PortBinding {
                            host_port: pb
                                .host_port
                                .or(Some(pb.container_port))
                                .as_ref()
                                .map(ToString::to_string),
                            host_ip: pb.host_ip.as_ref().map(ToString::to_string),
                        }]),
                    ),
                )
            })
            .unzip(),
    )
    .unzip()
}
