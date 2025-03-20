use std::time::Duration;

use stacked_errors::{ensure, ensure_eq, Result, StackableErr};
use super_orchestrator::{
    cli_docker::{Container, ContainerNetwork, Dockerfile},
    net_message::wait_for_ok_lookup_host,
};
use tracing::info;

const BASE_CONTAINER: &str = "fedora:41";
const TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    // note that you need the `DEBUG` level to see some of the debug output when it
    // is enabled
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    let logs_dir = "./logs";

    info!("\n\nexample 0\n");

    // a default container configuration with the `BASE_CONTAINER` image
    let container = Container::new("example0", Dockerfile::name_tag(BASE_CONTAINER));

    // set log files to be generated in `logs_dir` and for the entrypoint and
    // entrypoint args of the container to be equivalent to running "ls -a /" from a
    // shell inside the container
    let container = container.log(true).entrypoint("/usr/bin/ls", ["-a", "/"]);

    // run the container in debug mode
    let comres = container.run(None, TIMEOUT, logs_dir, true).await.stack()?;
    // and get the output as if it were run locally
    comres.assert_success().stack()?;
    dbg!(comres.stdout_as_utf8().stack()?);
    ensure!(!comres.stdout.is_empty());
    ensure!(comres.stderr.is_empty());

    info!("\n\nexample 1\n");

    // test that stderr and errors are handled correctly
    let comres = Container::new("example1", Dockerfile::name_tag(BASE_CONTAINER))
        .log(true)
        .entrypoint("/usr/bin/ls", ["-a", "/nonexistent"])
        .run(None, TIMEOUT, logs_dir, false)
        .await
        .stack()?;
    ensure!(!comres.successful());
    dbg!(comres.stderr_as_utf8().stack()?);
    ensure!(comres.stdout.is_empty());
    ensure!(!comres.stderr.is_empty());

    info!("\n\nexample 2\n");

    // sleep for 1 second
    Container::new("example2", Dockerfile::name_tag(BASE_CONTAINER))
        .entrypoint("/usr/bin/sleep", ["1"])
        .run(None, TIMEOUT, logs_dir, false)
        .await
        .stack()?
        .assert_success()
        .stack()?;

    info!("\n\nexample 3\n");

    // purposely timeout
    let comres = Container::new("example3", Dockerfile::name_tag(BASE_CONTAINER))
        .entrypoint("/usr/bin/sleep", ["infinity"])
        .run(None, Duration::from_secs(1), logs_dir, false)
        .await;
    dbg!(&comres);
    ensure!(comres.unwrap_err().is_timeout());

    info!("\n\nexample 4\n");

    // read from a local folder that is mapped to the container's filesystem with a
    // volume
    let comres = Container::new("example4", Dockerfile::name_tag(BASE_CONTAINER))
        .entrypoint("/usr/bin/cat", ["/dockerfile_resources/example.txt"])
        .volume(
            "./dockerfiles/dockerfile_resources/",
            "/dockerfile_resources/",
        )
        .run(None, TIMEOUT, logs_dir, false)
        .await
        .stack()?;
    comres.assert_success().stack()?;
    ensure_eq!(comres.stdout_as_utf8().stack()?, "hello from example.txt");

    info!("\n\nexample 5\n");

    // for more complicated things we need `ContainerNetwork`s
    let mut cn = ContainerNetwork::new("test", None, logs_dir);
    cn.add_container(
        Container::new("example5", Dockerfile::name_tag(BASE_CONTAINER))
            .entrypoint("/usr/bin/sleep", ["3"]),
    )
    .stack()?;
    // run all containers
    cn.run_all().await.stack()?;

    // when communicating inside a container to another container in the same
    // network, you can use the `container_name` of the container as the
    // hostname, in this case "example5"

    // but outside the network we need the IP address
    let host_ip = cn
        .wait_get_ip_addr(20, Duration::from_millis(300), "example5")
        .await
        .stack()?;
    info!("{}", &host_ip);

    // use port 0 to just detect that the host container exists
    wait_for_ok_lookup_host(2, Duration::from_millis(300), &format!("{host_ip:?}:0"))
        .await
        .stack()?;

    // wait for all containers to stop
    cn.wait_with_timeout_all(true, TIMEOUT).await.stack()?;
    // always run this at the end, ensuring the containers are logically terminated
    cn.terminate_all().await;

    info!("test completed successfully");

    Ok(())
}
