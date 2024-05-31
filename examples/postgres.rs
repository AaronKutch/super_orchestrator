//! This example has a postgres container with a volume to `./logs/pg_data` (of
//! course, in a real setup there would be a separate resources directory) that
//! persists data between runs. It should say "Database directory appears to
//! contain a database; Skipping initialization" on the second run. Run the
//! `clean` binary to reset.

use std::time::Duration;

use clap::Parser;
use super_orchestrator::{
    acquire_dir_path,
    docker::{Container, ContainerNetwork, Dockerfile},
    sh,
    stacked_errors::{Error, Result, StackableErr},
    wait_for_ok, Command,
};
use tokio::{fs, time::sleep};
use tracing::info;

// time until the program ends after everything is deployed
const END_TIMEOUT: Duration = Duration::from_secs(1_000_000_000);

#[rustfmt::skip]
fn test_dockerfile() -> String {
    let dynamic = "something";
    format!(
        r#"FROM fedora:38

# dependencies for `psql`
RUN dnf install -y postgresql libpq-devel

# After predetermined setups are when dynamic things should be placed,
# in order to maximize the amount that Docker can cache.

ENV SOMETHING="example/{dynamic}"
"#
    )
}

#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    #[arg(long)]
    entry_name: Option<String>,
    #[arg(long)]
    uuid: Option<String>,
    #[arg(long, default_value_t = String::from("./logs/"))]
    pg_data_base_path: String,
    #[arg(long, default_value_t = String::from("pg_data/"))]
    pg_data_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();

    if let Some(ref s) = args.entry_name {
        match s.as_str() {
            "test_runner" => test_runner(&args).await,
            _ => Err(Error::from(format!("entry_name \"{s}\" is not recognized"))),
        }
    } else {
        container_runner(&args).await.stack()
    }
}

async fn container_runner(args: &Args) -> Result<()> {
    let logs_dir = "./logs";
    let dockerfiles_dir = "./dockerfiles";
    let bin_entrypoint = "postgres";
    let container_target = "x86_64-unknown-linux-gnu";

    // build internal runner with `--release`
    //sh("cargo build --release --bin", &[
    //    bin_entrypoint,
    //    "--target",
    //    container_target,
    //])
    //.await.stack()?;
    //let entrypoint =
    // &format!("./target/{container_target}/release/{bin_entrypoint}");

    // for this example we need this command
    sh([
        "cargo build --release --example",
        bin_entrypoint,
        "--target",
        container_target,
    ])
    .await
    .stack()?;
    let entrypoint = &format!("./target/{container_target}/release/examples/{bin_entrypoint}");

    // we can't put the directory in source control with the .gitignore trick,
    // because postgres doesn't like the .gitignore
    let mut pg_data_path = acquire_dir_path(&args.pg_data_base_path)
        .await
        .stack_err(|| "you need to run from the repo root")?;
    pg_data_path.push(&args.pg_data_dir);
    if acquire_dir_path(&pg_data_path).await.is_err() {
        fs::create_dir_all(&pg_data_path).await.stack()?;
    }

    let containers = vec![
        Container::new("test_runner", Dockerfile::contents(test_dockerfile()))
            .external_entrypoint(entrypoint, ["--entry-name", "test_runner"])
            .await
            .stack()?
            // if exposing a port beyond the machine, use something like this on the
            // container
            .create_args(["-p", "127.0.0.1:8000:8000"]),
    ];

    let mut cn =
        ContainerNetwork::new("test", containers, Some(dockerfiles_dir), true, logs_dir).stack()?;
    cn.add_common_volumes([(logs_dir, "/logs")]);
    let uuid = cn.uuid_as_string();
    cn.add_common_entrypoint_args(["--uuid", &uuid]);

    // Adding the postgres container afterwards so that it doesn't receive all the
    // common flags.

    // NOTE: weird things happen if volumes to the same container overlap, e.g. if
    // the local logs directory were added when the `pg_data` directory is also in
    // the same local logs directory.
    #[rustfmt::skip]
    cn.add_container(
        Container::new(
            "postgres",
            Dockerfile::name_tag("postgres:16"),
        )
        .volume(
            pg_data_path.to_str().stack()?,
            "/var/lib/postgresql/data",
        )
        .environment_vars([
            ("POSTGRES_PASSWORD", "root"),
            ("POSTGRES_USER", "postgres"),
            // this conveniently causes postgres to create a
            // database of this name if it is not already
            // existing in the data directory
            ("POSTGRES_DB", "my_database"),
            // arguments like this may be needed
            ("POSTGRES_INITDB_ARGS", "-E UTF8 --locale=C"),
        ])
        // if you want to have a stable `http://postgres:5432/` address instead of
        // just `http://postgres_{uuid}:5432/` (both can still be used though),
        // but know there will be address conflicts
        .no_uuid_for_host_name(),
    )
    .stack()?;

    cn.run_all(true).await.stack()?;

    // for long runs
    //cn.wait_with_timeout_all(true, END_TIMEOUT).await.stack()?;

    cn.wait_with_timeout(&mut vec!["test_runner".to_owned()], true, END_TIMEOUT)
        .await
        .stack()?;

    cn.terminate_all().await;

    info!("test done");

    Ok(())
}

async fn test_runner(args: &Args) -> Result<()> {
    let uuid = &args.uuid.as_deref().stack()?;

    async fn postgres_health(uuid: &str) -> Result<()> {
        Command::new(format!(
            "psql --host=postgres_{uuid} -U postgres --command=\\l"
        ))
        .env("PGPASSWORD", "root")
        .run_to_completion()
        .await
        .stack()?
        .assert_success()
        .stack()?;
        Ok(())
    }
    wait_for_ok(10, Duration::from_secs(1), || postgres_health(uuid))
        .await
        .stack()?;

    // check that no uuid host works
    Command::new("psql --host=postgres -U postgres --command=\\l")
        .env("PGPASSWORD", "root")
        .debug(true)
        .run_to_completion()
        .await
        .stack()?
        .assert_success()
        .stack()?;

    info!("postgres is ready");

    // for long runs
    //sleep(END_TIMEOUT).await;

    sleep(Duration::ZERO).await;

    Ok(())
}
