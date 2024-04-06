#![allow(clippy::assigning_clones)]

use std::path::{Path, PathBuf};

use stacked_errors::{Error, Result, StackableErr};
use tokio::fs;

// Note: we use `dunce::simplify` because of https://github.com/rust-lang/rust/issues/42869
// and because we want to use `tokio::fs`.

/// Canonicalizes and checks the existence of a path. Also adds on better
/// information to errors.
///
/// Note: this does not prevent TOCTOU bugs. See the crate examples for more.
pub async fn acquire_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    // note: we don't need fs::try_exists because the canonicalization deals with
    // testing for existence and the symbolic links

    let mut path = fs::canonicalize(path)
        .await
        .stack_err(|| format!("acquire_path(path: {:?})", path))?;
    if cfg!(windows) {
        path = dunce::simplified(&path).to_owned();
    }
    Ok(path)
}

/// Canonicalizes and checks the existence of a file path. Also adds on better
/// information to errors.
///
/// Note: this does not prevent TOCTOU bugs. See the crate examples for more.
pub async fn acquire_file_path(file_path: impl AsRef<Path>) -> Result<PathBuf> {
    let file_path = file_path.as_ref();
    let mut path = fs::canonicalize(file_path)
        .await
        .stack_err(|| format!("acquire_file_path(file_path: {:?})", file_path))?;
    if cfg!(windows) {
        path = dunce::simplified(&path).to_owned();
    }
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::from(format!(
            "acquire_file_path(file_path: {:?}) -> is not a file",
            file_path
        )))
    }
}

/// Canonicalizes and checks the existence of a directory path. Also adds on
/// better information to errors.
///
/// Note: this does not prevent TOCTOU bugs. See the crate examples for more.
pub async fn acquire_dir_path(dir_path: impl AsRef<Path>) -> Result<PathBuf> {
    let dir_path = dir_path.as_ref();
    let mut path = fs::canonicalize(dir_path)
        .await
        .stack_err(|| format!("acquire_dir_path(dir_path: {:?})", dir_path))?;
    if cfg!(windows) {
        path = dunce::simplified(&path).to_owned();
    }
    if path.is_dir() {
        Ok(path)
    } else {
        Err(Error::from(format!(
            "acquire_dir_path(dir_path: {:?}) -> is not a directory",
            dir_path
        )))
    }
}
