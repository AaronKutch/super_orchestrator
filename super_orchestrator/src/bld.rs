/// Configure the complete workflow for a container.
///
/// When thinking of typical production environment, "declaring" a container is
/// composed of two main commands: `docker build` and `docker create`. This
/// module is an opinionated system for creating an image (output of build) but
/// the create command is not explored since it's usually provider specific. For
/// example the super_manager module has a struct
/// [SuperContainer](crate::bld::super_manager::SuperContainer) that configures
/// a container's create arguments to facilitate testing.
///
/// While docker files create a slightly variable build step (by using different
/// build args, different files being copied etc.) and `docker run` is also
/// variable (by using different run options), this module gives the caller the
/// ability to define and build a container with the same "arguments" and
/// (hopefully) result in a more reproducible way to create docker images.
pub mod super_docker_file;
/// Constructs for managing containers in a controlled environment.
/// Useful for creating integration tests and examples.
///
/// This module uses
/// [SuperDockerFile](crate::bld::super_docker_file::SuperDockerFile) to create
/// containers for testing and adds a simple way to declare docker networks,
/// manage conatainers in the networks and check the outputs for effective
/// testing.
pub mod super_manager;

pub mod docker_socket {
    use std::sync::{LazyLock, OnceLock};

    use stacked_errors::{Result, StackableErr};

    pub async fn get_or_init_default_docker_instance() -> Result<bollard::Docker> {
        static DOCKER_SOCKET: OnceLock<bollard::Docker> = OnceLock::new();
        static EXEC_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

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
