//! This example has a postgres container with a volume to `./logs/pg_data` (of
//! course, in a real setup there would be a separate resources directory) that
//! persists data between runs. It should say "Database directory appears to
//! contain a database; Skipping initialization" on the second run. Run the
//! `clean` binary to reset.

use std::time::Duration;

use clap::Parser;
use stacked_errors::{bail, Result, StackableErr};
use super_orchestrator::{
    acquire_dir_path,
    cli_docker::{Container, ContainerNetwork, Dockerfile},
    sh, wait_for_ok, Command,
};
use tokio::{fs, time::sleep};
use tracing::info;

const POSTGRES: &str = "postgres:18";
const BASE_CONTAINER: &str = "fedora:43";
// musl builds are more portable because it's statically linked
//
// When testing with x86_64-unknown-linux-gnu, if container had older glibc
// version, compared to host, the rust binary would not run.
const TARGET: &str = "x86_64-unknown-linux-musl";
const TIMEOUT: Duration = Duration::from_secs(3600);

fn test_dockerfile() -> String {
    let dynamic = "something";
    format!(
        r#"FROM {BASE_CONTAINER}

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
            "test_runner" => test_runner().await,
            _ => bail!("entry_name \"{s}\" is not recognized"),
        }
    } else {
        container_runner(&args).await.stack()
    }
}

async fn container_runner(args: &Args) -> Result<()> {
    let logs_dir = "./logs";
    let dockerfiles_dir = "./dockerfiles";
    let bin_entrypoint = "postgres";
    let container_target = TARGET;

    // build internal runner with `--release`
    sh([
        "cargo build --release --bin",
        bin_entrypoint,
        "--target",
        container_target,
    ])
    .await
    .stack()?;
    let entrypoint = &format!("./target/{container_target}/release/{bin_entrypoint}");

    // we can't put the directory in source control with the .gitignore trick,
    // because postgres doesn't like the .gitignore
    let mut pg_data_path = acquire_dir_path(&args.pg_data_base_path)
        .await
        .stack_err("you need to run from the repo root")?;
    pg_data_path.push(&args.pg_data_dir);
    if acquire_dir_path(&pg_data_path).await.is_err() {
        fs::create_dir_all(&pg_data_path).await.stack()?;
    }

    let mut cn = ContainerNetwork::new("test", Some(dockerfiles_dir), logs_dir);
    // display all of the build steps
    cn.debug_all(true);
    cn.add_container(
        Container::new("test_runner", Dockerfile::contents(test_dockerfile()))
            .external_entrypoint(entrypoint, ["--entry-name", "test_runner"])
            .await
            .stack()?
            //.build_args(["--network=host"])
            // if exposing a port beyond the machine, use something like this on the
            // container
            //.create_args(["-p", "0.0.0.0:5432:5432"]),
            .create_args(["-p", "127.0.0.1:5432:5432"]),
    )
    .stack()?;

    // NOTE: weird things happen if volumes to the same container overlap, e.g. if
    // the local logs directory were added when the `pg_data` directory is also in
    // the same local logs directory.
    #[rustfmt::skip]
    cn.add_container(
        Container::new(
            "postgres",
            Dockerfile::name_tag(POSTGRES),
        )
        .volume(
            pg_data_path.to_str().stack()?,
            // note: this is the directory to mount as of Postgres 18+
            "/var/lib/postgresql",
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
    )
    .stack()?;

    cn.add_common_volumes([(logs_dir, "/logs")]);

    cn.run_all().await.stack()?;

    // only wait on the "test_runner" because the postgres container will run by
    // itself forever
    cn.wait_with_timeout(["test_runner"], true, TIMEOUT)
        .await
        .stack()?;

    cn.terminate_all().await;

    info!("test done");

    Ok(())
}

async fn test_runner() -> Result<()> {
    async fn postgres_health() -> Result<()> {
        Command::new("psql --host=postgres -U postgres --command=\\l")
            .env("PGPASSWORD", "root")
            .run_to_completion()
            .await
            .stack()?
            .assert_success()
            .stack()?;
        Ok(())
    }
    wait_for_ok(10, Duration::from_secs(1), postgres_health)
        .await
        .stack()?;

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
    //sleep(TIMEOUT).await;

    sleep(Duration::ZERO).await;

    info!("stopping");

    Ok(())
}
