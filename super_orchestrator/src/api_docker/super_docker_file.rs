use std::{
    io::{Seek, Write},
    path::PathBuf,
    sync::Arc,
};

use bollard::image::BuildImageOptions;
use futures::TryStreamExt;
use stacked_errors::{Result, StackableErr};

use crate::{
    api_docker::{
        docker_socket, resolve_from_to, BootstrapOptions, SuperBuildImageOptionsWrapper,
        SuperImage, SuperTarballWrapper,
    },
    cli_docker::Dockerfile,
    sh,
};

/// Describes all the details needed to create and run a reproducible container
/// via the Docker API.
///
/// See the documentation for the [CLI version](crate::cli_docker::Container)
/// first. Dockerfiles are a definition that is still variable at `docker build`
/// time (by using different build args, having different files that a local
/// "COPY" command uses, etc). Even the later CLI `docker create` is also
/// variable due to voluming and some provider specific things. The API version
/// of this instead includes these special external file arguments in a tarball,
/// and its singular build step command has most of the things that `docker
/// create` could do. This API version wrapper does not expose a `docker create`
/// equivalent, since its remaining uses are provider-specific and hinder
/// reproducibility. The `docker create` equivalent options can still be set in
/// [SuperContainer](crate::bld::super_manager::SuperContainer) if desired.
///
/// Use [DockerFile] to define a "base" for it. All further function
/// calls simply add options to the build command, prepare a tarball that will
/// be used to seamlesly build the container, or push lines to the
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

async fn create_docker_file_returning_file_handle(sdf: &SuperDockerFile) -> Result<std::fs::File> {
    let mut temp_file_name = std::env::temp_dir();
    temp_file_name.push(uuid::Uuid::new_v4().to_string());

    let file_contents = match &sdf.base {
        Dockerfile::NameTag(nt) => Ok(format!("FROM {nt}").into_bytes()),
        Dockerfile::Path(path) => std::fs::read(path).stack(),
        Dockerfile::Contents(content) => Ok(content.clone().into_bytes()),
    }
    .map(|mut df| {
        df.extend_from_slice(&sdf.content_extend);
        df
    })
    .stack()?;

    tracing::trace!(
        "Creating container using docker file:\n{}",
        String::from_utf8_lossy(&file_contents)
    );

    tokio::task::spawn_blocking(move || {
        let mut temp_file = std::fs::File::options()
            .truncate(true)
            .create(true)
            .write(true)
            .read(true)
            .open(&temp_file_name)
            .stack()?;

        temp_file.write_all(&file_contents).stack()?;

        temp_file.seek(std::io::SeekFrom::Start(0)).stack()?;

        Ok(temp_file)
    })
    .await
    .stack()?
}

impl SuperDockerFile {
    #[tracing::instrument(skip_all, fields(
        image.name = ?image_name
    ))]
    pub fn new(base: Dockerfile, image_name: Option<String>) -> Self {
        Self {
            base,
            content_extend: Vec::new(),
            build_opts: SuperBuildImageOptionsWrapper::default(),
            tarball: Default::default(),
            image_name,
            build_path: None,
        }
    }

    #[tracing::instrument(skip_all, fields(
        image.name = ?image_name
    ))]
    pub fn new_with_tar(
        base: Dockerfile,
        image_name: Option<String>,
        tarball: Vec<u8>,
    ) -> Result<Self> {
        Ok(Self {
            base,
            image_name,
            content_extend: Vec::new(),
            build_opts: SuperBuildImageOptionsWrapper::default(),
            tarball: SuperTarballWrapper::new(tarball).stack()?,
            build_path: None,
        })
    }

    /// The build path is the last argument in a docker build command.
    ///
    /// `docker build [OPTS] <build_path>`
    ///
    /// If you are copying relative files, they will be copied relative to
    /// the current build_path which resolves to cwd if not specified
    /// (absolute paths don't apply). So specify this before copying or defining
    /// entrypoint to have paths resolved according to build path.
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub fn with_build_path(mut self, build_path: PathBuf) -> Self {
        self.build_path = Some(build_path);
        self
    }

    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub fn with_build_opts(mut self, build_opts: SuperBuildImageOptionsWrapper) -> Self {
        self.build_opts = build_opts;
        self
    }

    /// Add instructions to the underlying docker file
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub fn appending_dockerfile_instructions(
        mut self,
        v: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        self.appending_dockerfile_lines_mut(v);
        self
    }

    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub fn appending_dockerfile_lines_mut(&mut self, v: impl IntoIterator<Item = impl AsRef<str>>) {
        for s in v {
            self.content_extend.push(b'\n');

            self.content_extend.extend(s.as_ref().as_bytes());
        }
    }

    /// Adds a `COPY` instruction to the dockerfile, copying a file at a file
    /// path into memory. The argument receives an iterator with items as
    /// `(host_source_path, image_destination_path)`.
    ///
    /// As long as it returns `Ok(_)`, there cannot be TOCTOU problems.
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub async fn copying_from_paths(
        mut self,
        v: impl IntoIterator<Item = (impl ToString, impl ToString)>,
    ) -> Result<Self> {
        let build_path = self.build_path.clone();

        tracing::debug!("Current tarball paths: {:?}", self.tarball);

        let this = Arc::new(std::sync::Mutex::new(self));
        let mut futs = v
            .into_iter()
            .map(|(from, to)| {
                let this = this.clone();
                let (from, to) = resolve_from_to(from, to, build_path.clone());

                tokio::task::spawn_blocking(move || {
                    let file = &mut std::fs::File::open(&from).stack()?;

                    let mut this_ref = this.lock().unwrap();

                    this_ref.appending_dockerfile_lines_mut([format!("COPY {from} {to}")]);
                    this_ref.tarball.append_file(from, file).stack()?;

                    Ok(()) as Result<_>
                })
            })
            .collect::<Vec<_>>();

        while !futs.is_empty() {
            let (res, _, rest) = futures::future::select_all(futs).await;
            res.stack()?.stack()?;
            futs = rest;
        }

        self = Arc::try_unwrap(this).unwrap().into_inner().stack()?;

        tracing::debug!("New tarball paths: {:?}", self.tarball);

        Ok(self)
    }

    /// Adds a `COPY` instruction to the dockerfile, copying the contents of the
    /// arguments to memory. The items are of the form `(destination_path, mode,
    /// content)`
    ///
    /// Where mode is the unix access modes octaves 0oXXX, defaults to 777
    pub async fn copying_from_contents(
        mut self,
        v: impl IntoIterator<Item = (impl ToString, Option<u32>, Vec<u8>)>,
    ) -> Result<Self> {
        tracing::debug!("Current tarball paths: {:?}", self.tarball);

        let this = Arc::new(std::sync::Mutex::new(self));
        let mut futs = v
            .into_iter()
            .map(|(to, mode, content)| {
                let this = this.clone();
                let to = to.to_string();

                tokio::task::spawn_blocking(move || {
                    let mut this_ref = this.lock().unwrap();

                    this_ref.appending_dockerfile_lines_mut([format!("COPY {to} {to}")]);
                    this_ref
                        .tarball
                        .append_file_bytes(to, mode.unwrap_or(0o777), &content)
                        .stack()?;

                    Ok(()) as Result<_>
                })
            })
            .collect::<Vec<_>>();

        while !futs.is_empty() {
            let (res, _, rest) = futures::future::select_all(futs).await;
            res.stack()??;
            futs = rest;
        }

        self = Arc::try_unwrap(this).unwrap().into_inner().stack()?;

        tracing::debug!("New tarball paths: {:?}", self.tarball);

        Ok(self)
    }

    /// Add an `ENTRYPOINT` instruction and append its file to docker "build
    /// tarball".
    ///
    /// The entrypoint parameter is of the format (from, to).
    ///
    /// If you already have an entrypoint and need to just change args, use
    /// [SuperDockerFile::appending_dockerfile_instructions].
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub async fn with_entrypoint(
        mut self,
        entrypoint: (impl ToString, impl ToString),
        entrypoint_args: impl IntoIterator<Item = impl ToString>,
    ) -> Result<Self> {
        let entrypoint = (entrypoint.0.to_string(), entrypoint.1.to_string());
        self = self
            .copying_from_paths([entrypoint.clone()])
            .await
            .stack()?;
        let (_, to) = resolve_from_to(entrypoint.0, entrypoint.1, self.build_path.clone());

        let entrypoint_args = entrypoint_args.into_iter().collect::<Vec<_>>();
        let entrypoint_args = (!entrypoint_args.is_empty())
            .then(|| {
                ", ".to_string()
                    + &entrypoint_args
                        .into_iter()
                        .map(|s| format!("\"{}\"", s.to_string()))
                        .collect::<Vec<String>>()
                        .join(", ")
            })
            .unwrap_or_default();

        Ok(self.appending_dockerfile_instructions([format!(
            r#"ENTRYPOINT ["{to}"{entrypoint_args}] "#,
        )]))
    }

    /// Make the current running binary the image's entrypoint, will call
    /// [SuperDockerFile::with_entrypoint]. If `to` is None, will create file as
    /// /super-bootstrapped
    ///
    /// This is useful for defining a complete test using a single rust file by
    /// traversing through different branches of the code using the
    /// entrypoint_args.
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub async fn bootstrap(
        self,
        to: impl ToString,
        entrypoint_args: impl IntoIterator<Item = impl ToString>,
    ) -> Result<Self> {
        let bootstrap_path = to;

        let binary_path = std::env::current_exe()
            .stack()?
            .to_str()
            .stack()?
            .to_string();

        tracing::info!("Using path as entrypoint: {binary_path}");

        self.with_entrypoint((binary_path, bootstrap_path), entrypoint_args)
            .await
    }

    /// Similar to bootstrap but if the current target is not
    /// x86_64-unknown-linux-musl, build and use musl binary else use
    /// current binary. This is useful because musl is more portable and overall
    /// will just work when using as container entrypoint.
    ///
    /// From cargo build --help, the relevant `target_selection_flag`s: --bin
    /// --example --test --bench
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name
    ))]
    pub async fn bootstrap_musl(
        self,
        to: impl ToString,
        entrypoint_args: impl IntoIterator<Item = impl ToString>,
        bootstrap_option: BootstrapOptions,
        other_build_flags: impl IntoIterator<Item = impl ToString>,
    ) -> Result<Self> {
        let target_selection_flag = bootstrap_option.to_flag();
        let musl_target_path = &mut vec!["target", "x86_64-unknown-linux-musl", "release"];

        if let Some(path) = bootstrap_option.to_path_str() {
            musl_target_path.push(path);
        }

        let mut cur_binary_path = std::env::current_exe().stack()?;
        let cur_binary_name = cur_binary_path
            .file_name()
            .unwrap()
            .to_str()
            .stack()?
            .to_string();
        cur_binary_path.pop();

        let mut is_musl = true;

        let musl_path_it = musl_target_path.iter().rev();
        for cur_path in musl_path_it {
            if !cur_binary_path.ends_with(cur_path) {
                is_musl = false;
                break;
            }

            cur_binary_path.pop();
        }

        let bootstrap_path = to;

        if !is_musl {
            tracing::debug!("Current binary is not linked with musl, building to accordingly");

            let build_flags = other_build_flags
                .into_iter()
                .map(|s| s.to_string())
                .collect::<Vec<String>>();
            sh([
                "cargo build -r --target x86_64-unknown-linux-musl",
                target_selection_flag,
                &cur_binary_name,
            ]
            .into_iter()
            .chain(build_flags.iter().map(String::as_str)))
            .await
            .stack()?;
            let entrypoint = &format!(
                "./target/x86_64-unknown-linux-musl/release{}/{cur_binary_name}",
                bootstrap_option
                    .to_path_str()
                    .map_or_else(Default::default, |path| format!("/{path}")),
            );

            self.with_entrypoint((entrypoint, bootstrap_path), entrypoint_args)
                .await
                .stack()
        } else {
            tracing::debug!("Current binary is linked with musl, using it!");
            self.bootstrap(bootstrap_path, entrypoint_args)
                .await
                .stack()
        }
    }

    /// Inserts the Dockerfile into the tarball and consumes Self returning the
    /// necessary arguments for calling [bollard::Docker::build_image].
    #[tracing::instrument(skip_all, fields(
        image.name = ?self.image_name,
    ))]
    pub async fn into_bollard_args(mut self) -> Result<(BuildImageOptions<String>, Vec<u8>)> {
        const DOCKER_FILE_NAME: &str = "./super.dockerfile";

        let docker_file = &mut create_docker_file_returning_file_handle(&self)
            .await
            .stack()?;

        self.tarball
            .append_file(DOCKER_FILE_NAME.to_string(), docker_file)
            .stack()?;

        if let Some(image_name) = self.image_name {
            let (key, val) = image_name
                .split_once(':')
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .unwrap_or((image_name, Default::default()));
            self.build_opts.labels.insert(key, val);
        }

        let opts = BuildImageOptions {
            labels: self.build_opts.labels,
            dockerfile: DOCKER_FILE_NAME.to_string(),
            t: self.build_opts.t,
            extrahosts: self.build_opts.extrahosts,
            q: self.build_opts.q,
            nocache: self.build_opts.nocache,
            cachefrom: self.build_opts.cachefrom,
            pull: self.build_opts.pull,
            rm: self.build_opts.rm,
            forcerm: self.build_opts.forcerm,
            memory: self.build_opts.memory,
            memswap: self.build_opts.memswap,
            cpushares: self.build_opts.cpushares,
            cpusetcpus: self.build_opts.cpusetcpus,
            cpuperiod: self.build_opts.cpuperiod,
            cpuquota: self.build_opts.cpuquota,
            buildargs: self.build_opts.buildargs,
            shmsize: self.build_opts.shmsize,
            squash: self.build_opts.squash,
            networkmode: self.build_opts.networkmode,
            platform: self.build_opts.platform,
            target: self.build_opts.target,
            version: self.build_opts.version,
            ..Default::default()
        };

        let tarball = self.tarball.into_tarball().stack()?;

        Ok((opts, tarball))
    }

    /// Calls [bollard::Docker::build_image] using return of
    /// [SuperDockerFile::into_bollard_args] and the default docker instance
    /// from [bollard::Docker::connect_with_defaults].
    pub async fn build_with_bollard_defaults(
        build_opts: BuildImageOptions<String>,
        tarball: Vec<u8>,
    ) -> Result<(SuperImage, Vec<u8>)> {
        let docker_instance = docker_socket::get_or_init_default_docker_instance()
            .await
            .stack()?;

        let image_id = docker_instance
            // need the clone here because of incompatibility with tar::Builder and bytes::BytesMut
            .build_image(build_opts, None, Some(tarball.clone().into()))
            .inspect_ok(|msg| {
                msg.stream
                    .as_ref()
                    .inspect(|x| tracing::debug!("{}", x.trim()));
            })
            .try_filter_map(|x| futures::future::ready(Ok(x.aux)))
            .try_collect::<Vec<_>>()
            .await
            .stack_err("try to build img")?
            .into_iter()
            .next()
            .and_then(|x| x.id)
            .stack_err("image built without id")?;

        Ok((SuperImage::new(image_id), tarball))
    }

    /// Calls [SuperDockerFile::build_with_bollard_defaults] using the arguments
    /// returned from [SuperDockerFile::into_bollard_args].
    pub async fn build_image(self) -> Result<(SuperImage, Vec<u8>)> {
        let (build_opts, tarball) = self.into_bollard_args().await.stack()?;

        Self::build_with_bollard_defaults(build_opts, tarball)
            .await
            .stack()
    }
}
