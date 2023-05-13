use tokio::fs::{File, OpenOptions};

use crate::{acquire_dir_path, MapAddError, Result};

#[derive(Debug, Clone)]
pub struct LogFileOptions {
    /// Directory where the log file will reside
    pub directory: String,
    /// File name
    pub file_name: String,
    /// Whether to create the file if needed or expect a preexisting one
    pub create: bool,
    /// Whether to overwrite the file contents. Has the effect of
    /// `OpenOptions::new().truncate(self.overwrite)`
    pub overwrite: bool,
}

impl LogFileOptions {
    pub fn new(directory: &str, file_name: &str, create: bool, overwrite: bool) -> Self {
        Self {
            directory: directory.to_owned(),
            file_name: file_name.to_owned(),
            create,
            overwrite,
        }
    }

    pub async fn acquire_file(&self) -> Result<File> {
        let dir_path = acquire_dir_path(&self.directory)
            .await
            .map_add_err(|| format!("{self:?}.acquire_file()"))?;
        let mut file_path = dir_path.clone();
        file_path.push(&self.file_name);
        let mut oo = OpenOptions::new();
        oo.write(true);
        oo.create(self.create);
        if self.overwrite {
            oo.truncate(true);
        } else {
            oo.append(true);
        }
        oo.open(&file_path)
            .await
            .map_add_err(|| format!("{self:?}.acquire_file()"))
    }
}
