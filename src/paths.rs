use std::path::{Path, PathBuf};

use stacked_errors::{Error, Result, StackableErr};
use tokio::fs;

/// Canonicalizes and checks the existence of a path. Also adds on better
/// information to errors.
///
/// Note: this does not prevent TOCTOU bugs. See the crate examples for more.
pub async fn acquire_path(path_str: impl AsRef<Path>) -> Result<PathBuf> {
    // note: we don't need fs::try_exists because the canonicalization deals with
    // testing for existence and the symbolic links
    let path = path_str.as_ref();
    fs::canonicalize(path)
        .await
        .stack_err(|| format!("acquire_path(path_str: {path:?})"))
}

/// Canonicalizes and checks the existence of a file path. Also adds on better
/// information to errors.
///
/// Note: this does not prevent TOCTOU bugs. See the crate examples for more.
pub async fn acquire_file_path(file_path_str: impl AsRef<Path>) -> Result<PathBuf> {
    let file_path_str = file_path_str.as_ref();
    let path = fs::canonicalize(file_path_str)
        .await
        .stack_err(|| format!("acquire_file_path(file_path_str: {file_path_str:?})"))?;
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::from(format!(
            "acquire_file_path(file_path_str: {file_path_str:?}) -> is not a file"
        )))
    }
}

/// Canonicalizes and checks the existence of a directory path. Also adds on
/// better information to errors.
///
/// Note: this does not prevent TOCTOU bugs. See the crate examples for more.
pub async fn acquire_dir_path(dir_path_str: impl AsRef<Path>) -> Result<PathBuf> {
    let file_path_str = dir_path_str.as_ref();
    let path = fs::canonicalize(file_path_str)
        .await
        .stack_err(|| format!("acquire_dir_path(dir_path_str: {file_path_str:?})"))?;
    if path.is_dir() {
        Ok(path)
    } else {
        Err(Error::from(format!(
            "acquire_dir_path(dir_path_str: {file_path_str:?}) -> is not a directory"
        )))
    }
}
