//! See README.md for more

mod command;
mod command_runner;
mod file_options;
mod misc;
mod parsing;

/// Docker container management, using the docker API provided by [bollard] as a
/// backend.
#[cfg(feature = "bollard")]
pub mod api_docker;
/// Docker container management, using the "docker" OS command as a backend.
///
/// See the `basic_containers`, `docker_entrypoint_pattern`, and `postgres`
/// crate examples. There is an alternative [api_docker] using the docker API
/// backend that can be enabled with the "bollard" feature.
pub mod cli_docker;
mod paths;
pub use command::*;
pub use command_runner::*;
/// Communication with `NetMessenger`
pub mod net_message;
pub use file_options::*;
pub use misc::*;
pub use parsing::*;
pub use paths::*;
