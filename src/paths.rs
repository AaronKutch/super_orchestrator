use std::path::PathBuf;

use stacked_errors::{Error, MapAddError, Result};
use tokio::fs;

/// Canonicalizes and checks the existence of a path. Also adds on better
/// information to errors. Note: may introduce TOCTOU bugs.
#[track_caller]
pub async fn acquire_path(path_str: &str) -> Result<PathBuf> {
    // note: we don't need fs::try_exists because the canonicalization deals with
    // testing for existence and the symbolic links
    let path = PathBuf::from(path_str);
    fs::canonicalize(&path)
        .await
        .map_add_err(|| format!("acquire_path(path_str: \"{path_str}\")"))
}

/// Canonicalizes and checks the existence of a file path. Also adds on better
/// information to errors. Note: may introduce TOCTOU bugs.
#[track_caller]
pub async fn acquire_file_path(file_path_str: &str) -> Result<PathBuf> {
    let path = PathBuf::from(file_path_str);
    let path = fs::canonicalize(&path)
        .await
        .map_add_err(|| format!("acquire_file_path(file_path_str: \"{file_path_str}\")"))?;
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::from(format!(
            "acquire_file_path(file_path_str: \"{file_path_str}\") -> is not a file"
        )))
    }
}

/// Canonicalizes and checks the existence of a directory path. Also adds on
/// better information to errors. Note: may introduce TOCTOU bugs.
#[track_caller]
pub async fn acquire_dir_path(dir_path_str: &str) -> Result<PathBuf> {
    let path = PathBuf::from(dir_path_str);
    let path = fs::canonicalize(&path)
        .await
        .map_add_err(|| format!("acquire_dir_path(dir_path_str: \"{dir_path_str}\")"))?;
    if path.is_dir() {
        Ok(path)
    } else {
        Err(Error::from(format!(
            "acquire_dir_path(dir_path_str: \"{dir_path_str}\") -> is not a directory"
        )))
    }
}
