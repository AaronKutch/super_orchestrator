use super_orchestrator::{
    acquire_file_path, remove_files_in_dir, stacked_errors::Result, std_init, FileOptions,
};

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;

    // remove special temporary
    remove_files_in_dir("./dockerfiles", &["__tmp.dockerfile"]).await?;
    // remove log files only
    remove_files_in_dir("./logs", &[".log"]).await?;

    // test the unit test

    // create some empty example files
    FileOptions::write_str("./logs/binary", "").await?;
    FileOptions::write_str("./logs/ex0.log", "").await?;
    FileOptions::write_str("./logs/ex1.log", "").await?;
    FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
    FileOptions::write_str("./logs/tar.gz", "").await?;

    remove_files_in_dir("./logs", &["r.gz", ".r.gz"]).await?;
    // check that files "ex2.tar.gz" and "tar.gz" were not removed
    // even though "r.gz" is in their string suffixes, because it
    // only matches against complete extension components.
    acquire_file_path("./logs/ex2.tar.gz").await?;
    acquire_file_path("./logs/tar.gz").await?;

    remove_files_in_dir("./logs", &["binary", ".log"]).await?;
    // check that only the "binary" and all ".log" files were removed
    assert!(acquire_file_path("./logs/binary").await.is_err());
    assert!(acquire_file_path("./logs/ex0.log").await.is_err());
    assert!(acquire_file_path("./logs/ex1.log").await.is_err());
    acquire_file_path("./logs/ex2.tar.gz").await?;
    acquire_file_path("./logs/tar.gz").await?;

    remove_files_in_dir("./logs", &[".gz"]).await?;
    // any thing ending with ".gz" should be gone
    assert!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
    assert!(acquire_file_path("./logs/tar.gz").await.is_err());

    // recreate some files
    FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
    FileOptions::write_str("./logs/ex3.tar.gz.other", "").await?;
    FileOptions::write_str("./logs/tar.gz", "").await?;

    remove_files_in_dir("./logs", &["tar.gz"]).await?;
    // only the file is matched because the element did not begin with a "."
    acquire_file_path("./logs/ex2.tar.gz").await?;
    acquire_file_path("./logs/ex3.tar.gz.other").await?;
    assert!(acquire_file_path("./logs/tar.gz").await.is_err());

    FileOptions::write_str("./logs/tar.gz", "").await?;

    remove_files_in_dir("./logs", &[".tar.gz"]).await?;
    // only a strict extension suffix is matched
    assert!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
    acquire_file_path("./logs/ex3.tar.gz.other").await?;
    acquire_file_path("./logs/tar.gz").await?;

    FileOptions::write_str("./logs/ex2.tar.gz", "").await?;

    remove_files_in_dir("./logs", &[".gz", ".other"]).await?;
    assert!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
    assert!(acquire_file_path("./logs/ex3.tar.gz.other").await.is_err());
    assert!(acquire_file_path("./logs/tar.gz").await.is_err());

    Ok(())
}
