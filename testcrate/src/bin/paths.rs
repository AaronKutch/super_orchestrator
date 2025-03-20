use stacked_errors::{ensure, Result, StackableErr};
use super_orchestrator::{acquire_dir_path, acquire_file_path, acquire_path};

#[rustfmt::skip]
#[tokio::main]
async fn main() -> Result<()> {
    // often we can use it just to check for the existence of a path and return a
    // stacked error if there is an issue.
    acquire_path("./logs/").await.stack()?;
    acquire_path("./logs/.gitignore").await.stack()?;

    ensure!(
        acquire_path("./logs/.gitignore")
            .await
            .unwrap()
            .ends_with("super_orchestrator/logs/.gitignore")
    );

    // normalization is performed, note it always returns an absolute path but we
    // are testing only the ends for testing purposes.
    ensure!(
        acquire_path("./logs/../logs/../logs/.gitignore")
            .await
            .unwrap()
            .ends_with("super_orchestrator/logs/.gitignore")
    );

    ensure!(
        acquire_path("./logs/nonexistent.rs").await.is_err()
    );

    // the `_dir_` version insures it is only a directory
    acquire_dir_path("./logs/").await.stack()?;

    ensure!(
        acquire_dir_path("./logs")
            .await
            .unwrap()
            .ends_with("super_orchestrator/logs")
    );

    ensure!(
        acquire_path("./logs/../logs/../logs/")
            .await
            .unwrap()
            .ends_with("super_orchestrator/logs/")
    );

    ensure!(acquire_dir_path("./nonexistent").await.is_err());

    ensure!(
        acquire_dir_path("./logs/.gitignore").await.is_err()
    );

    // the `_file_` version insures it is only a file
    acquire_file_path("./logs/.gitignore").await.stack()?;

    ensure!(
        acquire_file_path("./logs/.gitignore")
            .await
            .unwrap()
            .ends_with("super_orchestrator/logs/.gitignore")
    );

    ensure!(
        acquire_file_path("./logs/../logs/../logs/.gitignore")
            .await
            .unwrap()
            .ends_with("super_orchestrator/logs/.gitignore")
    );

    ensure!(acquire_file_path("./nonexistent").await.is_err());

    ensure!(acquire_file_path("./logs").await.is_err());

    Ok(())
}
