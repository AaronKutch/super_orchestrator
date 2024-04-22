use clap::Parser;
use stacked_errors::Result;
use super_orchestrator::{ctrlc_init, docker_helpers::auto_exec};

/// Runs `super_orchestrator::docker_helpers::auto_exec`
#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    /// Prefix of the name of the container
    #[arg(short, long)]
    prefix: String,
    /// Adds the `-t` argument to use a TTY, may be needed on Windows to get
    /// around issues with carriage returns being passed.
    #[arg(short, long, default_value_t = false)]
    tty: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    ctrlc_init().unwrap();
    let args = Args::parse();
    auto_exec(if args.tty { ["-it"] } else { ["-i"] }, &args.prefix, [
        "sh",
    ])
    .await?;
    Ok(())
}
