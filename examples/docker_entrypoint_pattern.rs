//! Note: change working directory to this crate's root in two separate
//! terminals. In one terminal, run
//! `cargo r --example auto_exec_i -- --container-name container0`
//! and in the other `cargo r --example docker_entrypoint_pattern`. The first
//! terminal should catch a running container, and you can run commands on it or
//! ctrl-c to end the container early. The second will finish after building and
//! 20 seconds.

use std::time::Duration;

use clap::Parser;
use log::info;
use stacked_errors::{MapAddError, Result};
use super_orchestrator::{
    docker::{Container, ContainerNetwork, Dockerfile},
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
    entry_name: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;
    let args = Args::parse();

    if let Some(ref s) = args.entry_name {
        match s.as_str() {
            "container0" => container0_runner().await,
            "container1" => container1_runner().await,
            "container2" => Ok(()),
            _ => format!("entrypoint \"{s}\" is not recognized").map_add_err(|| ()),
        }
    } else {
        container_runner().await
    }
}

async fn container_runner() -> Result<()> {
    let logs_dir = "./logs";
    let dockerfiles_dir = "./dockerfiles";
    let bin_entrypoint = "docker_entrypoint_pattern";
    let container_target = "x86_64-unknown-linux-gnu";

    // build internal runner with `--release`
    //sh("cargo build --release --bin", &[
    //    bin_entrypoint,
    //    "--target",
    //    container_target,
    //])
    //.await?;
    //let entrypoint =
    // &format!("./target/{container_target}/release/{bin_entrypoint}");

    // for this example we need this command
    sh("cargo build --release --example", &[
        bin_entrypoint,
        "--target",
        container_target,
    ])
    .await?;
    let entrypoint = Some(format!(
        "./target/{container_target}/release/examples/{bin_entrypoint}"
    ));
    let entrypoint = entrypoint.as_deref();

    let mut cn = ContainerNetwork::new(
        "test",
        vec![
            Container::new(
                "container0",
                Dockerfile::NameTag("fedora:38".to_owned()),
                entrypoint,
                &["--entrypoint", "container0"],
            ),
            Container::new(
                "container1",
                Dockerfile::Path(format!("{dockerfiles_dir}/example.dockerfile")),
                entrypoint,
                &["--entrypoint", "container1"],
            ),
            Container::new(
                "container2",
                // note: when adding from a local file, the file must be located in the same
                // directory as the temporary dockerfile, which is why ".../dockerfile_resources"
                // is under the `dockerfile_write_dir`
                Dockerfile::Contents(
                    "FROM fedora:38\nADD ./dockerfile_resources/.gitignore /tmp/example.txt\n"
                        .to_owned(),
                ),
                entrypoint,
                &["--entrypoint", "container2"],
            ),
        ],
        Some(dockerfiles_dir),
        // TODO see issue on `ContainerNetwork` struct documentation
        true,
        logs_dir,
    )?
    // check the local ./logs directory
    .add_common_volumes(&[(logs_dir, "/logs")]);
    cn.run_all(true).await?;

    // container2 ends early
    cn.wait_with_timeout(&mut vec!["container2".to_owned()], true, TIMEOUT)
        .await?;
    assert_eq!(cn.active_names(), &["container0", "container1"]);
    assert_eq!(cn.inactive_names(), &["container2"]);

    info!("waiting on rest of containers to finish");
    cn.wait_with_timeout_all(true, TIMEOUT).await?;
    Ok(())
}

async fn container0_runner() -> Result<()> {
    let host = "container1:26000";
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
