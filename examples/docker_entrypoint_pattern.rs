//! Note: change working directory to this crate's root in two separate
//! terminals. In one terminal, run
//! `cargo r --example auto_exec_i -- --container-name container0_runner`
//! and in the other `cargo r --example docker_entrypoint_pattern`. The first
//! terminal should catch a running container, and you can run commands on it or
//! ctrl-c to end the container early. The second will finish after building and
//! 20 seconds.

use std::time::Duration;

use clap::Parser;
use log::info;
use stacked_errors::{MapAddError, Result};
use super_orchestrator::{
    docker::{Container, ContainerNetwork},
    net_message::NetMessenger,
    sh, std_init, STD_DELAY, STD_TRIES,
};
use tokio::time::sleep;

const TIMEOUT: Duration = Duration::from_secs(300);

/// Runs `docker_entrypoint_pattern`
#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    /// If left `None`, the container runner program runs, otherwise this
    /// specifies the entrypoint to run
    #[arg(short, long)]
    entrypoint: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;
    let args = Args::parse();

    if let Some(ref s) = args.entrypoint {
        match s.as_str() {
            "container0_runner" => container0_runner().await,
            "container1_runner" => container1_runner().await,
            _ => format!("entrypoint \"{s}\" is not recognized").map_add_err(|| ()),
        }
    } else {
        container_runner().await
    }
}

async fn container_runner() -> Result<()> {
    let container_target = "x86_64-unknown-linux-gnu";
    let logs_dir = "./logs";
    let this_bin = "docker_entrypoint_pattern";

    // build internal runner with `--release`
    //sh("cargo build --release --bin", &[
    //    this_bin,
    //    "--target",
    //    container_target,
    //])
    //.await?;
    //let entrypoint = &format!("./target/{container_target}/release/{this_bin}");

    // for this example we need this command
    sh("cargo build --release --example", &[
        this_bin,
        "--target",
        container_target,
    ])
    .await?;
    let entrypoint = &format!("./target/{container_target}/release/examples/{this_bin}");

    let volumes = &[("./logs", "/logs")];
    let mut cn = ContainerNetwork::new(
        "test",
        vec![
            Container::new(
                "container0_runner",
                None,
                // note: you would put a path to a docker file above if you wanted to run that way
                // and set this field to `None`, otherwise if you want the plain image do this
                Some("fedora:38"),
                &[],
                volumes,
                entrypoint,
                &["--entrypoint", "container0_runner"],
            ),
            Container::new(
                "container1_runner",
                None,
                Some("fedora:38"),
                &[],
                volumes,
                entrypoint,
                &["--entrypoint", "container1_runner"],
            ),
        ],
        // TODO see issue on `ContainerNetwork` struct documentation
        true,
        logs_dir,
    )?;
    cn.run_all(true).await?;
    cn.wait_with_timeout_all(true, TIMEOUT).await?;
    Ok(())
}

async fn container0_runner() -> Result<()> {
    let host = "container1_runner:26000";
    let mut nm = NetMessenger::connect(STD_TRIES, STD_DELAY, host)
        .await
        .map_add_err(|| ())?;
    let s = "hello world".to_owned();
    info!("container 0 runner is waiting for 20 seconds");
    sleep(Duration::from_secs(20)).await;
    nm.send::<String>(&s).await?;
    Ok(())
}

async fn container1_runner() -> Result<()> {
    let host = "0.0.0.0:26000";
    let mut nm = NetMessenger::listen_single_connect(host, TIMEOUT).await?;
    info!("container 1 runner is waiting to get something from container 0");
    let s: String = nm.recv().await?;
    info!("container 1 received \"{s}\"");
    assert_eq!(&s, "hello world");
    Ok(())
}
