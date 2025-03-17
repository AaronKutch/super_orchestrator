use std::collections::HashMap;

pub use bollard::image::BuilderVersion;

/// Mostly copy pasted from
/// [BuildImageOptions](bollard::image::BuildImageOptions), removing some fields
/// that are handled by [SuperDockerFile](super::SuperDockerFile) and fields
/// that are not handled by this implementation.
#[derive(Debug, Clone, Default)]
pub struct SuperBuildImageOptionsWrapper {
    /// A name and optional tag to apply to the image in the `name:tag` format.
    /// If you omit the tag the default `latest` value is assumed. You can
    /// provide several `t` parameters.
    pub t: String,
    /// Extra hosts to add to `/etc/hosts`.
    pub extrahosts: Option<String>,
    /// Suppress verbose build output.
    pub q: bool,
    /// Do not use the cache when building the image.
    pub nocache: bool,
    /// JSON array of images used for build cache resolution.
    pub cachefrom: Vec<String>,
    /// Attempt to pull the image even if an older image exists locally.
    pub pull: bool,
    /// Remove intermediate containers after a successful build.
    pub rm: bool,
    /// Always remove intermediate containers, even upon failure.
    pub forcerm: bool,
    /// Set memory limit for build.
    pub memory: Option<u64>,
    /// Total memory (memory + swap). Set as `-1` to disable swap.
    pub memswap: Option<i64>,
    /// CPU shares (relative weight).
    pub cpushares: Option<u64>,
    /// CPUs in which to allow execution (e.g., `0-3`, `0,1`).
    pub cpusetcpus: String,
    /// The length of a CPU period in microseconds.
    pub cpuperiod: Option<u64>,
    /// Microseconds of CPU time that the container can get in a CPU period.
    pub cpuquota: Option<u64>,
    /// JSON map of string pairs for build-time variables. Users pass these
    /// values at build-time. Docker uses the buildargs as the environment
    /// context for commands run via the `Dockerfile` RUN instruction, or
    /// for variable expansion in other `Dockerfile` instructions.
    pub buildargs: HashMap<String, String>,
    /// Size of `/dev/shm` in bytes. The size must be greater than 0. If
    /// omitted the system uses 64MB.
    pub shmsize: Option<u64>,
    /// Squash the resulting images layers into a single layer.
    pub squash: bool,
    /// Arbitrary key/value labels to set on the image, as a JSON map of string
    /// pairs.
    pub labels: HashMap<String, String>,
    /// Sets the networking mode for the run commands during build. Supported
    /// standard values are: `bridge`, `host`, `none`, and
    /// `container:<name|id>`. Any other value is taken as a custom network's
    /// name to which this container should connect to.
    pub networkmode: String,
    /// Platform in the format `os[/arch[/variant]]`
    pub platform: String,
    /// Target build stage
    pub target: String,
    /// Builder version to use
    pub version: BuilderVersion,
}
