use std::time::Duration;

use stacked_errors::{ensure, ensure_eq, Result, StackableErr};
use super_orchestrator::{
    docker::{Container, ContainerNetwork, Dockerfile},
    net_message::wait_for_ok_lookup_host,
    FileOptions,
};

const TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let logs_dir = "./logs";

    println!("\n\nexample 0\n");

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

    println!("\n\nexample 1\n");

    // sleep for 1 second
    Container::new("container0", Dockerfile::name_tag("fedora:38"))
        .entrypoint("/usr/bin/sleep", ["1"])
        .run(None, TIMEOUT, logs_dir, true)
        .await
        .stack()?
        .assert_success()
        .stack()?;

    println!("\n\nexample 2\n");

    // purposely timeout
    let comres = Container::new("container0", Dockerfile::name_tag("fedora:38"))
        .entrypoint("/usr/bin/sleep", ["infinity"])
        .run(None, Duration::from_secs(1), logs_dir, true)
        .await;
    dbg!(&comres);
    ensure!(comres.unwrap_err().is_timeout());

    println!("\n\nexample 3\n");

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

    println!("\n\nexample 4\n");

    // for more complicated things we need `ContainerNetwork`s
    let mut cn = ContainerNetwork::new(
        "test",
        vec![
            Container::new("container0", Dockerfile::name_tag("fedora:38"))
                .entrypoint("/usr/bin/sleep", ["3"]),
        ],
        None,
        true,
        logs_dir,
    )
    .stack()?;
    // run all containers
    cn.run_all(true).await.stack()?;

    let uuid = cn.uuid_as_string();
    // when communicating inside a container to another container in the network,
    // you would use a hostname like this
    let host = format!("container0_{uuid}");
    dbg!(&host);
    // but outside we need the IP address
    let host_ip = cn
        .wait_get_ip_addr(20, Duration::from_millis(300), "container0")
        .await
        .stack()?;
    dbg!(&host_ip);

    // use port 0 to just detect that the host container exists
    wait_for_ok_lookup_host(2, Duration::from_millis(300), &format!("{host_ip:?}:0"))
        .await
        .stack()?;

    // wait for all containers to stop
    cn.wait_with_timeout_all(true, TIMEOUT).await.stack()?;
    // always run this at the end, ensuring the containers are logically terminated
    cn.terminate_all().await;

    Ok(())
}
