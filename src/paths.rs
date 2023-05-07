use std::path::PathBuf;

use tokio::fs;

use crate::{Error, Result};

/// Canonicalizes and checks the existence of a path. Also adds on better
/// information to errors. Note: may introduce TOCTOU bugs.
#[track_caller]
pub async fn acquire_path(path_str: &str) -> Result<PathBuf> {
    // note: we don't need fs::try_exists because the canonicalization deals with
    // testing for existence and the symbolic links
    let path = PathBuf::from(path_str);
    match fs::canonicalize(&path).await {
        Ok(path) => Ok(path),
        Err(e) => {
            // "No such file or directory" is garbage, add on path that caused it
            Err(Error::from(e).add_error(format!("acquire_path(path_str: \"{path_str}\") -> ")))
        }
    }
}

/// Canonicalizes and checks the existence of a file path. Also adds on better
/// information to errors. Note: may introduce TOCTOU bugs.
#[track_caller]
pub async fn acquire_file_path(file_path_str: &str) -> Result<PathBuf> {
    let path = PathBuf::from(file_path_str);
    match fs::canonicalize(&path).await {
        Ok(path) => {
            if path.is_file() {
                Ok(path)
            } else {
                Err(Error::from(format!(
                    "acquire_file_path(file_path_str: \"{file_path_str}\") -> \"is not a file\""
                )))
            }
        }
        Err(e) => Err(Error::from(e).add_error(format!(
            "acquire_file_path(file_path_str: \"{file_path_str}\") -> "
        ))),
    }
}

/// Canonicalizes and checks the existence of a directory path. Also adds on
/// better information to errors. Note: may introduce TOCTOU bugs.
#[track_caller]
pub async fn acquire_dir_path(dir_path_str: &str) -> Result<PathBuf> {
    let path = PathBuf::from(dir_path_str);
    match fs::canonicalize(&path).await {
        Ok(path) => {
            if path.is_dir() {
                Ok(path)
            } else {
                Err(Error::from(format!(
                    "acquire_dir_path(dir_path_str: \"{dir_path_str}\") -> \"is not a directory\""
                )))
            }
        }
        Err(e) => Err(Error::from(e).add_error(format!(
            "acquire_dir_path(dir_path_str: \"{dir_path_str}\") -> "
        ))),
    }
}
