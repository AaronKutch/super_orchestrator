use std::collections::HashSet;

use stacked_errors::{Result, StackableErr};

/// A tarball for directly placing files in a container at definition time
pub struct Tarball {
    tar: tar::Builder<Vec<u8>>,
    // TODO this was entirely for debug
    paths: HashSet<String>,
}

impl Default for Tarball {
    /// An empty tarball
    fn default() -> Self {
        Self {
            tar: tar::Builder::new(Vec::new()),
            paths: Default::default(),
        }
    }
}

// avoid the `tar::Builder`s
impl std::fmt::Debug for Tarball {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Tarball {{ {} }}",
            self.paths
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

impl Tarball {
    /// Uses the bytes of an existing tarball, also parsing and checking that
    /// the paths are valid UTF-8
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

    /// Append a file that will go to the given `path`, with `mode` and the
    /// bytes of the `content` of the file
    pub fn append_file_bytes(
        &mut self,
        path: impl ToString,
        mode: u32,
        content: &[u8],
    ) -> Result<()> {
        let header = &mut tar::Header::new_gnu();
        header.set_size(content.len() as _);
        header.set_mode(mode);
        header.set_cksum();
        self.tar
            .append_data(header, path.to_string(), content)
            .stack()
    }

    /// Uses a `std::fs::File` and its metadata
    pub fn append_file(&mut self, path: impl ToString, file: &mut std::fs::File) -> Result<()> {
        let path = path.to_string();
        self.paths.insert(path.clone());
        self.tar
            .append_file(path, file)
            .stack_err("Tarball::append_file")
    }

    /// Get the bytes of a tarball
    pub fn into_tarball(self) -> Result<Vec<u8>> {
        self.tar.into_inner().stack()
    }
}
