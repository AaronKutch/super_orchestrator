use std::collections::HashSet;

use stacked_errors::{Result, StackableErr};

pub struct SuperTarballWrapper {
    tar: tar::Builder<Vec<u8>>,
    paths: HashSet<String>,
}

impl Default for SuperTarballWrapper {
    fn default() -> Self {
        Self {
            tar: tar::Builder::new(Vec::new()),
            paths: Default::default(),
        }
    }
}

// avoid the `tar::Builder`s
impl std::fmt::Debug for SuperTarballWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SuperTarballWrapper {{ {} }}",
            self.paths
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

impl SuperTarballWrapper {
    pub fn new(tarball: Vec<u8>) -> Result<Self> {
        // rebuild paths (useful for debugging)
        let mut archive = tar::Archive::new(std::io::Cursor::new(&tarball));
        let mut paths = HashSet::new();
        for entry in archive.entries().stack()? {
            paths.insert(
                entry
                    .stack()?
                    .path()
                    .stack()?
                    .as_os_str()
                    .to_str()
                    .stack_err("failed to convert os_str to str")?
                    .to_string(),
            );
        }

        Ok(Self {
            tar: tar::Builder::new(tarball),
            paths,
        })
    }

    pub fn append_file_bytes(&mut self, path: String, mode: u32, content: &[u8]) -> Result<()> {
        let header = &mut tar::Header::new_gnu();
        header.set_size(content.len() as _);
        header.set_mode(mode);
        header.set_cksum();
        self.tar.append_data(header, path, content).stack()
    }

    pub fn append_file(&mut self, path: String, file: &mut std::fs::File) -> Result<()> {
        self.paths.insert(path.clone());
        self.tar.append_file(path, file).stack()
    }

    pub fn into_tarball(self) -> Result<Vec<u8>> {
        self.tar.into_inner().stack()
    }
}
