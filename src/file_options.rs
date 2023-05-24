use std::path::PathBuf;

use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
};

use crate::{acquire_dir_path, acquire_file_path, close_file, Error, MapAddError, Result};

#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    // creates file if nonexistent
    create: bool,
    // truncation by default, append otherwise
    append: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ReadOrWrite {
    Read,
    Write(WriteOptions),
}

/// A wrapper combining capabilities from `tokio::fs::{OpenOptions, File}` with
/// a lot of opinionated defaults and `close_file`.
#[derive(Debug, Clone)]
pub struct FileOptions {
    pub path: PathBuf,
    pub options: ReadOrWrite,
}

impl FileOptions {
    pub fn read(file_path: &str) -> Self {
        Self {
            path: PathBuf::from(file_path.to_owned()),
            options: ReadOrWrite::Read,
        }
    }

    pub fn read2(directory: &str, file_name: &str) -> Self {
        let mut path = PathBuf::from(directory.to_owned());
        path.push(file_name);
        Self {
            path,
            options: ReadOrWrite::Read,
        }
    }

    /// Sets `create` to true and `append` to false by default
    pub fn write(file_path: &str) -> Self {
        Self {
            path: PathBuf::from(file_path.to_owned()),
            options: ReadOrWrite::Write(WriteOptions {
                create: true,
                append: false,
            }),
        }
    }

    /// Sets `create` to true and `append` to false by default
    pub fn write2(directory: &str, file_name: &str) -> Self {
        let mut path = PathBuf::from(directory.to_owned());
        path.push(file_name);
        Self {
            path,
            options: ReadOrWrite::Write(WriteOptions {
                create: true,
                append: false,
            }),
        }
    }

    pub fn create(mut self, create: bool) -> Result<Self> {
        if let ReadOrWrite::Write(ref mut options) = self.options {
            options.create = create;
            Ok(self)
        } else {
            Err(Error::from(format!(
                "{self:?}.create() -> options are readonly"
            )))
        }
    }

    pub fn append(mut self, append: bool) -> Result<Self> {
        if let ReadOrWrite::Write(ref mut options) = self.options {
            options.append = append;
            Ok(self)
        } else {
            Err(Error::from(format!(
                "{self:?}.append() -> options are readonly"
            )))
        }
    }

    /// Checks only for existence of the directory and file (allowing the file
    /// to not exist if `create` is not true). Returns the combined path if
    /// `!create`, else returns the directory.
    pub async fn preacquire(&self) -> Result<PathBuf> {
        let dir = self
            .path
            .parent()
            .map_add_err(|| "FileOptions::preacquire() -> empty path")?
            .to_str()
            .map_add_err(|| "bad OsStr conversion")?;
        let path = acquire_dir_path(dir)
            .await
            .map_add_err(|| format!("{self:?}.preacquire() could not acquire directory"))?;
        match self.options {
            ReadOrWrite::Read => (),
            ReadOrWrite::Write(WriteOptions { create, .. }) => {
                if create {
                    return Ok(path)
                }
            }
        }
        acquire_file_path(path.to_str().map_add_err(|| "bad OsStr conversion")?)
            .await
            .map_add_err(|| {
                format!(
                    "{self:?}.precheck() could not acquire path to combined directory and file \
                     name"
                )
            })
    }

    pub async fn acquire_file(&self) -> Result<File> {
        let path = self
            .preacquire()
            .await
            .map_add_err(|| "FileOptions::acquire_file()")?;
        Ok(match self.options {
            ReadOrWrite::Read => OpenOptions::new()
                .read(true)
                .open(path)
                .await
                .map_add_err(|| format!("{self:?}.acquire_file()"))?,
            ReadOrWrite::Write(WriteOptions { create, append }) => {
                if create {
                    OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(!append)
                        .append(append)
                        .open(&path)
                        .await
                        .map_add_err(|| format!("{self:?}.acquire_file()"))?
                } else {
                    OpenOptions::new()
                        .write(true)
                        .truncate(!append)
                        .append(append)
                        .open(path)
                        .await
                        .map_add_err(|| format!("{self:?}.acquire_file()"))?
                }
            }
        })
    }

    pub async fn read_to_string(file_path: &str) -> Result<String> {
        let mut file = Self::read(file_path)
            .acquire_file()
            .await
            .map_add_err(|| "read_to_string")?;
        let mut s = String::new();
        file.read_to_string(&mut s).await?;
        Ok(s)
    }

    pub async fn read2_to_string(directory: &str, file_name: &str) -> Result<String> {
        let mut file = Self::read2(directory, file_name)
            .acquire_file()
            .await
            .map_add_err(|| "read2_to_string")?;
        let mut s = String::new();
        file.read_to_string(&mut s).await?;
        Ok(s)
    }

    pub async fn write_str(file_path: &str, s: &str) -> Result<()> {
        let mut file = Self::write(file_path)
            .acquire_file()
            .await
            .map_add_err(|| "write_str")?;
        file.write_all(s.as_bytes()).await.map_add_err(|| ())?;
        close_file(file).await.map_add_err(|| ())?;
        Ok(())
    }

    pub async fn write2_str(directory: &str, file_name: &str, s: &str) -> Result<()> {
        let mut file = Self::write2(directory, file_name)
            .acquire_file()
            .await
            .map_add_err(|| "write_str")?;
        file.write_all(s.as_bytes()).await.map_add_err(|| ())?;
        close_file(file).await.map_add_err(|| ())?;
        Ok(())
    }
}
