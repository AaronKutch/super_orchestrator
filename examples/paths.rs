use stacked_errors::{ensure, StackableErr};
use super_orchestrator::{
    acquire_dir_path, acquire_file_path, acquire_path, stacked_errors::Result,
};

#[rustfmt::skip]
#[tokio::main]
async fn main() -> Result<()> {
    // often we can use it just to check for the existence of a path and return a
    // stacked error if there is an issue.
    acquire_path("./examples/").await.stack()?;
    acquire_path("./examples/paths.rs").await.stack()?;

    ensure!(
        acquire_path("./examples/paths.rs")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples/paths.rs")
    );

    // normalization is performed, note it always returns an absolute path but we
    // are testing only the ends for testing purposes.
    ensure!(
        acquire_path("./examples/../examples/../examples/paths.rs")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples/paths.rs")
    );

    ensure!(
        acquire_path("./examples/nonexistent.rs").await.is_err()
    );

    // the `_dir_` version insures it is only a directory
    acquire_dir_path("./examples/").await.stack()?;

    ensure!(
        acquire_dir_path("./examples")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples")
    );

    ensure!(
        acquire_path("./examples/../examples/../examples/")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples/")
    );

    ensure!(acquire_dir_path("./nonexistent").await.is_err());

    ensure!(
        acquire_dir_path("./examples/paths.rs").await.is_err()
    );

    // the `_file_` version insures it is only a file
    acquire_file_path("./examples/paths.rs").await.stack()?;

    ensure!(
        acquire_file_path("./examples/paths.rs")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples/paths.rs")
    );

    ensure!(
        acquire_file_path("./examples/../examples/../examples/paths.rs")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples/paths.rs")
    );

    ensure!(acquire_file_path("./nonexistent").await.is_err());

    ensure!(acquire_file_path("./examples").await.is_err());

    Ok(())
}
