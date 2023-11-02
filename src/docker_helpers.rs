use std::{process::Stdio, time::Duration};

use log::{info, warn};
use stacked_errors::{Result, StackableErr};
use tokio::time::sleep;

use crate::{ctrlc_issued_reset, sh, Command, STD_DELAY};

/// Intended to be called from the main() of a standalone binary, or run from
/// this repo `cargo r --example auto_exec_i -- --container-name main`
///
/// This actively looks for a running container with the given
/// `container_name` prefix, and when such a container starts it gets the
/// container id and runs `docker exec -i [id] bash`, forwarding stdin and
/// stdout to whatever program is calling this. Using Ctrl-C causes this to
/// force terminate the container and resume looping. Ctrl-C again terminates
/// the whole program.
pub async fn auto_exec_i(container_name: &str) -> Result<()> {
    info!("running auto_exec_i({container_name})");
    loop {
        if ctrlc_issued_reset() {
            break
        }
        let comres = Command::new("docker ps", &[])
            .run_to_completion()
            .await
            .stack()?;
        comres.assert_success()?;
        let mut name_id = None;
        for line in comres.stdout_as_utf8().stack()?.lines().skip(1) {
            let line = line.trim();
            if let Some(inx) = line.rfind(container_name) {
                let name = &line[inx..];
                // make sure we are just getting the last field with the name
                if name.split_whitespace().nth(1).is_none() {
                    let id = &line[..line.find(' ').stack()?];
                    if name_id.is_some() {
                        warn!("Found multiple containers with same {name} prefix");
                        name_id = None;
                        break
                    }
                    name_id = Some((name.to_owned(), id.to_owned()));
                }
            }
        }
        if let Some((name, id)) = name_id {
            info!("Found container {name} with id {id}, forwarding stdin, stdout, stderr");
            docker_exec_i(&id).await.stack()?;
            let _ = sh("docker rm -f", &[&id]).await;
            info!("\nTerminated container {id}\n");
        }
        sleep(STD_DELAY).await;
    }
    Ok(())
}

pub async fn docker_exec_i(container_id: &str) -> Result<()> {
    let mut runner = Command::new("docker exec -i", &[container_id, "bash"])
        .debug(true)
        .run_with_stdin(Stdio::inherit())
        .await
        .stack()?;
    loop {
        if ctrlc_issued_reset() {
            runner.terminate().await.stack()?;
            break
        }
        match runner.wait_with_timeout(Duration::ZERO).await {
            Ok(()) => break,
            Err(e) => {
                if !e.is_timeout() {
                    runner.terminate().await.stack()?;
                    return e.stack()
                }
            }
        }
        sleep(STD_DELAY).await;
    }
    Ok(())
}
