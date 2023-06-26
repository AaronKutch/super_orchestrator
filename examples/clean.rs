use super_orchestrator::{
    acquire_file_path, remove_files_in_dir, stacked_errors::Result, std_init,
};
use tokio::fs::remove_file;

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;

    // remove special temporary
    remove_file(acquire_file_path("./dockerfiles/__tmp.dockerfile").await?).await?;
    // remove log files only
    remove_files_in_dir("./logs", &["log"]).await?;

    Ok(())
}
