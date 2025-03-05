mod bollard_wrappers;
mod impls;
mod super_tar;

use std::path::PathBuf;

pub use bollard_wrappers::SuperBuildImageOptionsWrapper;
use super_tar::SuperTarballWrapper;

use crate::docker_container::Dockerfile;

/// Use this to create a docker file representation equivalent. It's useful for
/// creating images.
///
/// Use [SuperDockerFile] to define a "base" for it. This base can be an image
/// label, the contents of a Dockerfile or the path of a Dockerfile, see
/// [Dockerfile](crate::docker_container::Dockerfile). All further function
/// calls simply add options to the build command, prepare a tarball that will
/// be used to seamleslly build the container or "extend" (push lines) to the
/// docker file.
#[derive(Debug)]
pub struct SuperDockerFile {
    base: Dockerfile,
    content_extend: Vec<u8>,
    tarball: SuperTarballWrapper,
    build_path: Option<PathBuf>,
    image_name: Option<String>,

    build_opts: SuperBuildImageOptionsWrapper,
}

/// Wrapper struct for the image, call [SuperImage::get_image_id] to get the id
/// of the image as a &str or [SuperImage::into_inner] to get the underlying
/// [String].
#[derive(Debug, Clone)]
pub struct SuperImage(String);

#[derive(Debug, Clone, Copy)]
pub enum BootstrapOptions {
    Example,
    Bin,
    Test,
    Bench,
}
