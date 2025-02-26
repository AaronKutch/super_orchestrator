//! This example has a postgres container with a volume to `./logs/pg_data` (of
//! course, in a real setup there would be a separate resources directory) that
//! persists data between runs. It should say "Database directory appears to
//! contain a database; Skipping initialization" on the second run. Run the
//! `clean` binary to reset.
//!
//! This is the rewrite of the postgres example using bollard backend.

use std::{path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;
use stacked_errors::{bail, Result, StackableErr};
use super_orchestrator::{
    acquire_dir_path,
    bld::{
        super_docker_file::{BootstrapOptions, SuperDockerFile},
        super_manager::{
            AddContainerOptions, OutputDirConfig, SuperContainerOptions, SuperCreateNetworkOptions,
            SuperNetwork, SUPER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME,
        },
    },
    docker::Dockerfile,
    wait_for_ok, Command,
};
use tokio::{fs, io::AsyncWriteExt};
use tracing::info;

const TEST_DOCKERFILE_CONTENT: &str = r#"FROM fedora:41

# dependencies for `psql`
RUN dnf install -y postgresql libpq-devel
"#;

#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    #[arg(long)]
    entry_name: Option<String>,
    #[arg(long, default_value_t = String::from("./logs/"))]
    pg_data_base_path: String,
    #[arg(long, default_value_t = String::from("pg_data/"))]
    pg_data_dir: String,
    #[arg(long, default_value_t = String::from("postgres"))]
    postgres_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();

    if let Some(ref s) = args.entry_name {
        match s.as_str() {
            "test_runner" => test_runner(args.postgres_name).await,
            _ => bail!("entry_name \"{s}\" is not recognized"),
        }
    } else {
        container_runner(&args).await.stack()
    }
}

async fn container_runner(args: &Args) -> Result<()> {
    let logs_dir = "./logs";

    // we can't put the directory in source control with the .gitignore trick,
    // because postgres doesn't like the .gitignore
    let mut pg_data_path = acquire_dir_path(&args.pg_data_base_path)
        .await
        .stack_err("you need to run from the repo root")?;
    pg_data_path.push(&args.pg_data_dir);
    if acquire_dir_path(&pg_data_path).await.is_err() {
        fs::create_dir_all(&pg_data_path).await.stack()?;
    }

    let mut cn = SuperNetwork::create(SuperCreateNetworkOptions {
        name: "test_postgres_bollard".to_string(),
        overwrite_existing: true,
        log_by_default: true,
        output_dir_config: Some(OutputDirConfig {
            output_dir: acquire_dir_path(logs_dir)
                .await
                .stack()?
                .to_str()
                .stack()?
                .to_string(),
            save_logs: true,
        }),
        ..Default::default()
    })
    .await
    .stack()?;

    let test_runner_name = super_orchestrator::random_name("test_runner".to_string());
    let postgres_name = super_orchestrator::random_name("postgres".to_string());

    let container_opts = SuperContainerOptions {
        name: test_runner_name.clone(),
        important: true,
        ..Default::default()
    };

    cn.add_container(
        AddContainerOptions::Container {
            image: SuperDockerFile::new(Dockerfile::contents(TEST_DOCKERFILE_CONTENT), None)
                .bootstrap_musl(None, [
                    "--entry-name",
                    "test_runner",
                    "--postgres-name",
                    &postgres_name,
                ], BootstrapOptions::Example,
                ["--features", "bollard"]
                )
                .await
                .stack()?
                .build_image()
                .await
                .stack()?
                .0,
        },
        Default::default(),
        container_opts,
    )
    .await
    .stack()?;

    cn.add_container(
        AddContainerOptions::DockerFile {
            docker_file: SuperDockerFile::new(Dockerfile::name_tag("postgres:16"), None)
                .appending_dockerfile_instructions([
                    "ENV POSTGRES_PASSWORD=root",
                    "ENV POSTGRES_USER=postgres",
                    // this conveniently causes postgres to create a
                    // database of this name if it is not already
                    // exis"ting in the data directory
                    "ENV POSTGRES_DB=my_database",
                ]),
        },
        Default::default(),
        SuperContainerOptions {
            name: postgres_name,
            volumes: [(
                pg_data_path.to_str().stack()?.to_string(),
                "/var/lib/postgresql/data".to_string(),
            )]
            .into(),
            priviledged: true,
            log_outs: Some(false),
            ..Default::default()
        },
    )
    .await
    .stack()?;

    cn.start_all().await.stack()?;

    cn.wait_important().await?;

    if let Err(err) = cn.teardown().await.stack() {
        tracing::warn!("{err}");
    }

    eprintln!("test done");

    let mut ok_file = PathBuf::from_str(logs_dir).stack()?;
    ok_file.push(test_runner_name);
    ok_file.push("ok");

    assert_eq!(tokio::fs::read_to_string(ok_file).await.stack()?, "ok");

    Ok(())
}

async fn test_runner(postgres_name: String) -> Result<()> {
    async fn postgres_health(postgres_name: &str) -> Result<()> {
        Command::new(format!(
            "psql --host={postgres_name} -U postgres --command=\\l"
        ))
        .env("PGPASSWORD", "root")
        .run_to_completion()
        .await
        .stack()?
        .assert_success()
        .stack()?;
        Ok(())
    }
    wait_for_ok(10, Duration::from_secs(1), || {
        postgres_health(&postgres_name)
    })
    .await
    .stack()?;

    Command::new(format!(
        "psql --host={postgres_name} -U postgres --command=\\l"
    ))
    .env("PGPASSWORD", "root")
    .debug(true)
    .run_to_completion()
    .await
    .stack()?
    .assert_success()
    .stack()?;

    info!("postgres is ready");

    let mut ok_file =
        PathBuf::from_str(&std::env::var(SUPER_NETWORK_OUTPUT_DIR_ENV_VAR_NAME).unwrap())
            .stack()?;
    ok_file.push("ok");

    tokio::fs::File::options()
        .write(true)
        .create(true)
        .open(ok_file)
        .await
        .stack()?
        .write_all(b"ok")
        .await
        .stack()?;

    Ok(())
}
