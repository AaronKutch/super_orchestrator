//! See README.md for more

mod command;
mod command_runner;
mod file_options;
mod misc;
mod parsing;

// TODO there is a bunch of rigor in `cli_docker` that has not yet been brought
// to `api_docker`, for instance there are no limiters on container output
// stored (actually, did we ever do that in `cli_docker`), there are not a lot
// of _timeout capabilities and there are certainly bugs with restarting
// containers and waiting on different sets of containers.

/// Docker container management, using the docker API provided by [bollard] as a
/// backend. NOTE: This is still experimental and subject to major bugs and
/// changes.
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
