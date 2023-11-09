use std::time::Duration;

use stacked_errors::{ensure, ensure_eq, Result, StackableErr};
use super_orchestrator::{
    docker::{Container, Dockerfile},
    std_init, FileOptions,
};

const TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;
    let logs_dir = "./logs";

    // a default container configuration with the fedora:38 image
    let container = Container::new("container0", Dockerfile::name_tag("fedora:38"));

    // solo run `/usr/bin/ls -a /` inside the container
    let comres = container
        .entrypoint("/usr/bin/ls", ["-a", "/"])
        .run(None, TIMEOUT, logs_dir, true)
        .await
        .stack()?;
    // and get the output as if it were run locally
    comres.assert_success().stack()?;
    dbg!(&comres.stdout_as_utf8().stack()?);

    // sleep for 1 second
    let comres = Container::new("container0", Dockerfile::name_tag("fedora:38"))
        .entrypoint("/usr/bin/sleep", ["1"])
        .run(None, TIMEOUT, logs_dir, true)
        .await
        .stack()?;
    comres.assert_success().stack()?;

    // purposely timeout
    let comres = Container::new("container0", Dockerfile::name_tag("fedora:38"))
        .entrypoint("/usr/bin/sleep", ["infinity"])
        .run(None, Duration::from_secs(1), logs_dir, true)
        .await;
    dbg!(&comres);
    ensure!(comres.unwrap_err().is_timeout());

    // read from a local folder that is mapped to the container's filesystem with a
    // volume
    let comres = Container::new("container0", Dockerfile::name_tag("fedora:38"))
        .entrypoint("/usr/bin/cat", ["/dockerfile_resources/example.txt"])
        .volume(
            "./dockerfiles/dockerfile_resources/",
            "/dockerfile_resources/",
        )
        .run(None, TIMEOUT, logs_dir, true)
        .await
        .stack()?;
    comres.assert_success().stack()?;
    ensure_eq!(
        comres.stdout_as_utf8().stack()?,
        FileOptions::read_to_string("./dockerfiles/dockerfile_resources/example.txt")
            .await
            .stack()?
    );

    Ok(())
}
