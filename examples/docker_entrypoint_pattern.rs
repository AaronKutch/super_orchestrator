//! Note: change working directory to this crate's root in two separate
//! terminals. In one terminal, run
//! `cargo r --example auto_exec -- --container-name container0`
//! and in the other `cargo r --example docker_entrypoint_pattern`. Note that on
//! windows you will need to use WSL 2 or else the cross compilation will fail
//! at linking stage. The first terminal should catch a running container, and
//! you can run commands on it or ctrl-c to end the container early. The second
//! will finish after building and 20 seconds.

use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};
use stacked_errors::{ensure_eq, Error, Result, StackableErr};
use super_orchestrator::{
    ctrlc_init,
    docker::{Container, ContainerNetwork, Dockerfile},
    net_message::NetMessenger,
    sh, FileOptions,
};
use tokio::time::sleep;
use tracing::info;

const BASE_CONTAINER: &str = "alpine:3.20";
// need this for Alpine
const TARGET: &str = "x86_64-unknown-linux-musl";

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
    /// In order to enable simultaneous `super_orchestrator` uses with the same
    /// names, UUIDs are appended to some things such as the hostname. This
    /// is used to pass the information around. This behavior can be overridden
    /// by `no_uuid_...` on individual containers or the whole network.
    #[arg(long)]
    uuid: Option<String>,
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
    // note that you need the `DEBUG` level to see some of the debug output when it
    // is enabled
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let mut args = Args::parse();
    if let Some(s) = &args.json_args {
        args = serde_json::from_str(s).stack()?;
    }

    if let Some(ref s) = args.entry_name {
        match s.as_str() {
            "container0" => container0_runner(&args).await.stack(),
            "container1" => container1_runner(&args).await.stack(),
            "container2" => container2_runner(&args).await.stack(),
            _ => Err(Error::from(format!("entrypoint \"{s}\" is not recognized"))),
        }
    } else {
        container_runner(&args).await.stack()
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
ADD ./dockerfile_resources/example.txt /resources/example.txt

# this is read by Clap
ENV ARG_FROM_ENV="environment var from dockerfile"
"##
    )
}

async fn container_runner(args: &Args) -> Result<()> {
    let logs_dir = "./logs";
    let dockerfiles_dir = "./dockerfiles";
    // note: some operating systems have incredibly bad error reporting when
    // incompatible binaries are accidentally used. If you see an error like "exec
    // /docker_entrypoint_pattern: no such file or directory" yet you can verify
    // there is in fact a file there, it turns out that if e.g. a GNU compiled
    // binary is used on a system that expects MUSL compiled binaries, then it will
    // say "no such file or directory" even though some file exists.
    let bin_entrypoint = "docker_entrypoint_pattern";
    let container_target = TARGET;

    // build internal runner with `--release`
    //sh([
    //    "cargo build --release --bin",
    //    bin_entrypoint,
    //    "--target",
    //    container_target,
    //])
    //.await
    //.stack()?;
    //let entrypoint =
    // &format!("./target/{container_target}/release/{bin_entrypoint}");

    // because this is an example we need a slightly different path
    sh([
        "cargo build --release --example",
        bin_entrypoint,
        "--target",
        container_target,
    ])
    .await
    .stack()?;
    let entrypoint = &format!("./target/{container_target}/release/examples/{bin_entrypoint}");

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

    let mut cn = ContainerNetwork::new("test", Some(dockerfiles_dir), logs_dir);

    // note that this turns on debug output for all stages, and also remember to set
    // the `tracing` consumer to display `DEBUG` level output
    cn.debug_all(true);

    // a container with a plain fedora:38 image
    cn.add_container(
        Container::new("container0", Dockerfile::name_tag(BASE_CONTAINER))
            .external_entrypoint(entrypoint, ["--entry-name", "container0"])
            .await
            .stack()?,
    )
    .stack()?;
    // uses the example dockerfile
    cn.add_container(
        Container::new(
            "container1",
            Dockerfile::path(format!("{dockerfiles_dir}/example.dockerfile")),
        )
        .external_entrypoint(entrypoint, ["--entry-name", "container1"])
        .await
        .stack()?,
    )
    .stack()?;
    // uses the literal string, allowing for self-contained complicated systems in a
    // single file
    cn.add_container(
        Container::new("container2", Dockerfile::contents(container2_dockerfile()))
            .external_entrypoint(entrypoint, container2_args)
            .await
            .stack()?,
    )
    .stack()?;

    // check the local "./logs" directory, this gets mapped to "/logs" inside the
    // containers
    cn.add_common_volumes([(logs_dir, "/logs")]);
    let uuid = cn.uuid_as_string();
    // passing UUID information through common arguments
    cn.add_common_entrypoint_args(["--uuid", &uuid]);

    // Whenever using the docker entrypoint pattern or similar setup where there is
    // a dedicated container runner function that is just calling
    // `wait_with_timeout` before `terminate_all` and exiting, `ctrlc_init`
    // should be used just before the `run_all`. This will then allow
    // `wait_with_timeout` the time to stop all containers before returning an
    // error, if a Ctrl+C or sigterm signal is issued. This may take a few moments.
    // Ctrl-C will work like intended in other cases and times.

    ctrlc_init().unwrap();

    cn.run_all().await.stack()?;

    // container2 ends early
    cn.wait_with_timeout(&mut vec!["container2".to_owned()], true, TIMEOUT)
        .await
        .stack()?;
    ensure_eq!(cn.active_names(), &["container0", "container1"]);
    ensure_eq!(cn.inactive_names(), &["container2"]);

    info!("waiting on rest of the containers to finish");
    cn.wait_with_timeout_all(true, TIMEOUT).await.stack()?;
    // there will be a warning if we do not properly terminate the container network
    // and there are still running containers or docker networks when the
    // `ContainerNetwork` is dropped
    cn.terminate_all().await;
    info!("test complete and cleaned up");
    Ok(())
}

async fn container0_runner(_args: &Args) -> Result<()> {
    // it might seem annoying to use `stack` at every fallible point, but this is
    // more than worth it when trying to decipher where an error is coming from
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
