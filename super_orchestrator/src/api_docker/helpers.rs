use std::{path::PathBuf, pin::Pin};

// reexport from bollard
pub use bollard::{
    container::LogOutput, errors::Error as BollardError, secret::ContainerWaitResponse,
};

/// The trait object for writing to a container's stdin
pub type DockerStdin = Pin<Box<dyn tokio::io::AsyncWrite + Send>>;

/// The trait object for receiving a container's stdout
pub type DockerOutput =
    Pin<Box<dyn futures::stream::Stream<Item = Result<LogOutput, BollardError>> + Send>>;

/// A callback style function for getting the container's stdout
pub type OutputHook = Box<dyn Fn(&Result<LogOutput, BollardError>) -> stacked_errors::Result<()>>;

/// The trait object for receiving a container's exit result
pub type WaitContainer = Pin<
    Box<dyn futures::stream::Stream<Item = Result<ContainerWaitResponse, BollardError>> + Send>,
>;

/// Things regarding the docker API socket
pub mod docker_socket {
    use std::sync::{LazyLock, OnceLock};

    use stacked_errors::{Result, StackableErr};

    /// This acquires a process-wide unified `bollard::Docker` handle
    pub async fn get_or_init_default_docker_instance() -> Result<bollard::Docker> {
        static DOCKER_SOCKET: OnceLock<bollard::Docker> = OnceLock::new();
        static EXEC_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

        // this has a fast path with a slow path that is careful to not block the
        // process
        if let Some(docker_instance) = DOCKER_SOCKET.get() {
            Ok(docker_instance.clone())
        } else {
            let _exec_lock = EXEC_LOCK.lock().await;

            if let Some(docker_instance) = DOCKER_SOCKET.get() {
                Ok(docker_instance.clone())
            } else {
                let docker_socket = tokio::task::spawn_blocking(|| {
                    bollard::Docker::connect_with_defaults().stack()
                })
                .await
                .stack()??;

                let _ = DOCKER_SOCKET.set(docker_socket);

                Ok(DOCKER_SOCKET.get().unwrap().clone())
            }
        }
    }
}

pub(crate) fn resolve_from_to(
    from: impl ToString,
    to: impl ToString,
    build_path: Option<PathBuf>,
) -> (String, String) {
    let from: String = if let Some(ref build_path) = build_path {
        build_path
            .join(from.to_string())
            .as_os_str()
            .to_str()
            .unwrap()
            .to_string()
    } else {
        from.to_string()
    };
    let to = to.to_string();

    (from, to)
}
