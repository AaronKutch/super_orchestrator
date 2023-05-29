use std::time::Duration;

use log::info;
use stacked_errors::{MapAddError, Result};
use tokio::time::sleep;

use crate::{ctrlc_issued_reset, sh, Command, STD_DELAY};

/// Intended to be called from the main() of a standalone binary, or run from
/// this repo `cargo r --example auto_exec_i -- --container-name main`
///
/// This actively looks for a running container with the given
/// `container_name`, and when such a container starts it gets the container id
/// and runs `docker exec -i [id] bash`, forwarding stdin and stdout to
/// whatever program is calling this. Using Ctrl-C causes this to force
/// terminate the container and resume looping. Ctrl-C again terminates the
/// whole program.
pub async fn auto_exec_i(container_name: &str) -> Result<()> {
    info!("running auto_exec_i({container_name})");
    loop {
        if ctrlc_issued_reset() {
            break
        }
        let comres = Command::new("docker ps", &[]).run_to_completion().await?;
        comres.assert_success()?;
        for line in comres.stdout.lines().skip(1) {
            if line.ends_with(container_name) {
                let line = line.trim();
                let id = &line[0..line.find(' ').map_add_err(|| ())?];
                info!("Found container {id}, forwarding stdin, stdout, stderr");
                docker_exec_i(id).await?;
                let _ = sh("docker rm -f", &[id]).await;
                info!("\nTerminated container {id}\n");
                break
            }
        }
        sleep(STD_DELAY).await;
    }
    Ok(())
}

pub async fn docker_exec_i(container_id: &str) -> Result<()> {
    let mut runner = Command::new("docker exec -i", &[container_id, "bash"])
        .ci_mode(true)
        .inherit_stdin(true)
        .run()
        .await?;
    loop {
        if ctrlc_issued_reset() {
            break
        }
        match runner.wait_with_timeout(Duration::ZERO).await {
            Ok(()) => break,
            Err(e) => {
                if !e.is_timeout() {
                    runner.terminate().await?;
                    return e.map_add_err(|| ())
                }
            }
        }
        sleep(STD_DELAY).await;
    }
    runner.terminate().await?;
    Ok(())
}
