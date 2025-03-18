//! Note: change working directory to this repo's root in two separate
//! terminals. In one terminal, run
//! `cargo r --bin auto_exec -- --prefix container0`
//! and in the other `cargo r --bin docker_entrypoint_pattern`. The
//! `auto_exec` binary will automatically attach to the container with the
//! matching prefix. Note that on windows you will need to use WSL 2 or else the
//! cross compilation will fail at linking stage. The first terminal should
//! catch a running container, and you can run commands on it or ctrl-c to end
//! the container early. The second will finish after building and 20 seconds.

use std::{str::FromStr, time::Duration};

use clap::Parser;
use serde::{Deserialize, Serialize};
use stacked_errors::{bail, ensure_eq, Result, StackableErr};
use super_orchestrator::{
    acquire_dir_path,
    api_docker::{
        AddContainerOptions, ContainerCreateOptions, ContainerNetwork, Dockerfile,
        NetworkCreateOptions, OutputDirConfig, SuperDockerfile, Tarball,
    },
    net_message::NetMessenger,
    FileOptions,
};
use tokio::time::sleep;
use tracing::info;
use tracing_subscriber::EnvFilter;

const BASE_CONTAINER: &str = "alpine:3.21";
// need this for Alpine
//const TARGET: &str = "x86_64-unknown-linux-musl";

const TIMEOUT: Duration = Duration::from_secs(300);
const STD_TRIES: u64 = 300;
const STD_DELAY: Duration = Duration::from_millis(300);

/// Runs `docker_entrypoint_pattern`
#[derive(Parser, Debug, Clone, Serialize, Deserialize)]
#[command(about)]
struct Args {
    /// If left `None`, the container runner program runs, otherwise this
    /// specifies the entrypoint to run
    #[arg(long)]
    entry_name: Option<String>,
    #[arg(long)]
    pass_along_example: Option<String>,
    /// needs the "env" Clap feature to compile
    #[arg(long, env)]
    arg_from_env: Option<String>,
    /// This prelude is used for booleans that should be false by default
    #[clap(
        long,
        default_missing_value("true"),
        default_value("false"),
        num_args(0..=1),
        action = clap::ArgAction::Set,
    )]
    pub boolean: bool,
    /// Parsed as `Args` and replaces everything if set
    #[arg(long)]
    pub json_args: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_str("trace,bollard=warn,hyper_util=info").unwrap())
        .init();

    let mut args = Args::parse();
    if let Some(s) = &args.json_args {
        let arg_from_env = args.arg_from_env;
        args = serde_json::from_str(s).stack()?;
        // except for the env var which we want to not override
        args.arg_from_env = arg_from_env;
    }

    if let Some(ref s) = args.entry_name {
        match s.as_str() {
            "container0" => container0_runner(&args).await,
            "container1" => container1_runner(&args).await,
            "container2" => container2_runner(&args).await,
            _ => bail!("entrypoint \"{s}\" is not recognized"),
        }
    } else {
        container_runner(&args).await
    }
}

// a dynamically generated dockerfile
fn container2_dockerfile() -> String {
    format!(
        r##"
FROM {BASE_CONTAINER}

# note: when adding from a local file, the file must be located under the same
# directory as the temporary dockerfile, which is why ".../dockerfile_resources"
# is under the `dockerfile_write_dir`
#ADD ./dockerfile_resources/example.txt /resources/example.txt

# this is read by Clap
ENV ARG_FROM_ENV="environment var from dockerfile"
"##
    )
}

async fn container_runner(args: &Args) -> Result<()> {
    let logs_dir = "./logs";
    let dockerfiles_dir = "./dockerfiles";
    // TODO the error compilation should detect this case

    // note: some operating systems have incredibly bad error reporting when
    // incompatible binaries are accidentally used. If you see an error like "exec
    // /docker_entrypoint_pattern: no such file or directory" yet you can verify
    // there is in fact a file there, it turns out that if e.g. a GNU compiled
    // binary is used on a system that expects MUSL compiled binaries, then it will
    // say "no such file or directory" even though some file exists.
    //let bin_entrypoint = "docker_entrypoint_pattern";
    //let container_target = TARGET;

    let mut example = FileOptions::read(format!(
        "{dockerfiles_dir}/dockerfile_resources/example.txt"
    ))
    .acquire_file()
    .await
    .stack()?
    .into_std()
    .await;
    let mut tarball = Tarball::default();
    tarball
        .append_file("./resources/example.txt", &mut example)
        .stack()?;

    let mut cn = ContainerNetwork::create(NetworkCreateOptions {
        name: "test".to_owned(),
        overwrite_existing: true,
        log_by_default: true,
        output_dir_config: Some(OutputDirConfig {
            output_dir: acquire_dir_path(logs_dir)
                .await
                .stack()?
                .to_str()
                .stack()?
                .to_string(),
            save_logs: true,
        }),
        ..Default::default()
    })
    .await
    .stack()?;

    // a container with a plain name:tag image
    cn.add_container(
        AddContainerOptions::DockerFile(SuperDockerfile::new(
            Dockerfile::name_tag(BASE_CONTAINER),
            None,
        )),
        Default::default(),
        ContainerCreateOptions {
            name: super_orchestrator::random_name("container0"),
            important: true,
            ..Default::default()
        },
    )
    .await
    .stack()?;

    // uses the example dockerfile
    cn.add_container(
        AddContainerOptions::DockerFile(SuperDockerfile::new(
            Dockerfile::path(format!("{dockerfiles_dir}/example.dockerfile")),
            None,
        )),
        Default::default(),
        ContainerCreateOptions {
            name: super_orchestrator::random_name("container1"),
            important: true,
            ..Default::default()
        },
    )
    .await
    .stack()?;

    // in more complicated uses of super_orchestrator, users will add one more
    // abstraction layer around `ContainerNetwork`s and `Container`s to
    // automatically deal with the preferred argument passing method, here we are
    // just writing them out

    // Container entrypoint binaries have no arguments passed to them by default. To
    // propogate all of the same arguments automatically, we clone the `Args` make
    // any changes we need, and serialize it to be sent through the `--json-args`
    // option. If we pass "--pass-along-example" when calling the container runner
    // subprocess, it will get propogated to all the containers as well.
    let mut container2_args = args.clone();
    // pass a different `entry_name` to each of them to tell them what to specialize
    // into, so that the whole system can be described by one file and one
    // cross compilation (this can of course be customized an infinite number of
    // ways, I have found this entrypoint pattern to be the most useful).
    container2_args.entry_name = Some("container2".to_owned());
    let container2_args = vec![
        "--json-args".to_owned(),
        serde_json::to_string(&container2_args).unwrap(),
    ];

    // uses `container2_dockerfile`, allowing for self-contained complicated systems
    // in a single file
    cn.add_container(
        AddContainerOptions::DockerFile(SuperDockerfile::new_with_tar(
            Dockerfile::contents(container2_dockerfile()),
            None,
            tarball,
        )),
        Default::default(),
        ContainerCreateOptions {
            name: super_orchestrator::random_name("container2"),
            important: true,
            env_vars: container2_args,
            ..Default::default()
        },
    )
    .await
    .stack()?;

    cn.teardown_on_ctrlc();

    cn.start_all().await.stack()?;

    cn.wait_important().await.stack()?;

    cn.teardown().await.stack()?;
    info!("test complete and cleaned up");
    Ok(())
}

async fn container0_runner(_args: &Args) -> Result<()> {
    // It might seem annoying to use `stack` at every fallible point, but this is
    // more than worth it when trying to decipher where an error is coming from. In
    // nontrivial async `tokio` usage, the backtraces get clobbered with task runner
    // functions, which is why I designed `stacked_errors` to enable programmed
    // backtraces.
    let mut nm = NetMessenger::connect(STD_TRIES, STD_DELAY, "container1:26000")
        .await
        .stack()?;
    let s = "hello world".to_owned();

    // check out the results of returning `stack_errors::Error`
    //let _ = super_orchestrator::FileOptions::read_to_string("./nonexistent")
    //    .await
    //    .stack()?;

    // check out the results of a panic
    //panic!("uh oh");

    info!("container 0 runner is waiting for 20 seconds before sending");
    sleep(Duration::from_secs(20)).await;
    nm.send::<String>(&s).await.stack()?;

    Ok(())
}

async fn container1_runner(_args: &Args) -> Result<()> {
    let host = "0.0.0.0:26000";
    let mut nm = NetMessenger::listen(host, TIMEOUT).await.stack()?;

    info!("container 1 runner is waiting to get something from container 0");
    let s: String = nm.recv().await.stack()?;
    info!("container 1 received \"{s}\"");

    // use `ensure` macros instead of of panicking assertions
    ensure_eq!(&s, "hello world");

    Ok(())
}

async fn container2_runner(args: &Args) -> Result<()> {
    info!(
        "the example passed argument is {:?}",
        args.pass_along_example
    );

    info!("the environment var is {:?}", args.arg_from_env);
    info!("the boolean is {:?}", args.boolean);
    eprintln!("testing stderr");

    // check that the file is in this container's filesystem
    ensure_eq!(
        FileOptions::read_to_string("/resources/example.txt")
            .await
            .stack()?,
        "hello from example.txt"
    );

    info!("container 2 is exiting");

    Ok(())
}
