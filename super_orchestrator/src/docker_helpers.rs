use std::{net::IpAddr, process::Stdio, time::Duration};

use stacked_errors::{bail, stacked_get, Result, StackableErr};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{ctrlc_issued_reset, sh, wait_for_ok, Command};

const STD_DELAY: Duration = Duration::from_millis(300);
const IP_RETRIES: u64 = 10;

/// Uses `docker inspect` to find the IP address of the container. There is a
/// delay between a container starting and an IP address being assigned, which
/// is why this has a retry mechanism.
pub async fn wait_get_ip_addr(
    num_retries: u64,
    delay: Duration,
    container_id: &str,
) -> Result<IpAddr> {
    async fn f(container_id: &str) -> Result<IpAddr> {
        let comres = Command::new("docker inspect")
            .arg(container_id)
            .run_to_completion()
            .await
            .stack_err("could not run `docker inspect`")?;
        comres
            .assert_success()
            .stack_err("get_ip_addr -> `docker inspect` was not successful")?;
        //println!("{}", comres.stdout_as_utf8().stack()?);
        let v: serde_json::Value =
            serde_json::from_str(comres.stdout_as_utf8().stack()?).stack()?;
        let networks = stacked_get!(v[0]["NetworkSettings"]["Networks"])
            .as_object()
            .stack()?;
        let network = networks.iter().next().stack()?.1;
        let addr = stacked_get!(network["IPAddress"]).as_str().stack()?;
        if addr.is_empty() {
            bail!("IP address has not been assigned yet")
        }
        let ip_addr: std::result::Result<IpAddr, _> = addr.parse();
        ip_addr.stack()
    }
    wait_for_ok(num_retries, delay, || f(container_id))
        .await
        .stack_err_with(|| format!("wait_get_ip_addr(container_id: {container_id})"))
}

/// Intended to be called from the main() of a standalone binary, or run from
/// this repo `cargo r --bin auto_exec -- --container-name main`
///
/// This actively looks for a running container with the given
/// `container_name` prefix, and when such a container starts it gets the
/// container id and runs `docker exec [exec_args..] [id] [container_args..`,
/// forwarding stdin and stdout to whatever program is calling this. Using
/// Ctrl-C causes this to force terminate the container and resume looping.
/// Ctrl-C again terminates the whole program. See the testcrate examples for more.
pub async fn auto_exec<I0, I1, S0, S1, S2>(
    exec_args: I0,
    container_name: S2,
    container_args: I1,
) -> Result<()>
where
    I0: IntoIterator<Item = S0>,
    S0: AsRef<str>,
    I1: IntoIterator<Item = S1>,
    S1: AsRef<str>,
    S2: AsRef<str>,
{
    let container_name = container_name.as_ref().to_string();
    let exec_args: Vec<String> = exec_args
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    let container_args: Vec<String> = container_args
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    info!("running auto_exec({exec_args:?} {container_name} {container_args:?})");
    loop {
        if ctrlc_issued_reset() {
            break
        }
        let comres = Command::new("docker ps")
            .run_to_completion()
            .await
            .stack()?;
        comres.assert_success()?;
        let mut name_id = None;
        for line in comres.stdout_as_utf8().stack()?.lines().skip(1) {
            let line = line.trim();
            if let Some(inx) = line.rfind(&container_name) {
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
            let ip = wait_get_ip_addr(IP_RETRIES, STD_DELAY, &id).await.stack();
            info!(
                "Found container {name} with id {id}, forwarding stdin, stdout, stderr.\nIP is: \
                 {ip:?}"
            );
            let mut total_args = exec_args.clone();
            total_args.push(id.to_string());
            total_args.extend(container_args.clone());
            docker_exec(total_args).await.stack()?;
            let _ = sh(["docker rm -f", &id]).await;
            info!("\nTerminated container {id}\n");
        }
        sleep(STD_DELAY).await;
    }
    Ok(())
}

pub async fn docker_exec<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut runner = Command::new("docker exec")
        .args(args.into_iter().map(|s| s.as_ref().to_string()))
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
