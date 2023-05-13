use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::time::sleep;

use crate::{sh, Command, MapAddError, Result};

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
    println!("running auto_exec_i({container_name})");

    let ctrlc_issued = Arc::new(AtomicBool::new(false));
    let ctrlc_issued_move = ctrlc_issued.clone();
    ctrlc::set_handler(move || {
        ctrlc_issued_move.store(true, Ordering::SeqCst);
    })?;

    loop {
        if ctrlc_issued.load(Ordering::SeqCst) {
            break
        }
        let comres = Command::new("docker ps", &[]).run_to_completion().await?;
        comres.assert_success()?;
        for line in comres.stdout.lines().skip(1) {
            if line.ends_with(container_name) {
                let line = line.trim();
                let id = &line[0..line.find(' ').map_add_err(|| ())?];
                println!("found container {id}, forwarding stdin, stdout, stderr");
                docker_exec_i(id, ctrlc_issued.clone()).await?;
                sh("docker rm -f", &[id]).await?;
                break
            }
        }
        sleep(Duration::from_millis(300)).await;
    }
    Ok(())
}

pub async fn docker_exec_i(container_id: &str, ctrlc_issued: Arc<AtomicBool>) -> Result<()> {
    let mut runner = Command::new("docker exec -i", &[container_id, "bash"])
        .ci_mode(true)
        .pipe_stdin(true)
        .run()
        .await?;
    loop {
        if ctrlc_issued.load(Ordering::SeqCst) {
            break
        }
        sleep(Duration::from_millis(300)).await;
    }
    runner.terminate().await?;
    Ok(())
}
