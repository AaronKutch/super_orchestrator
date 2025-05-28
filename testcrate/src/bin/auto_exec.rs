use clap::Parser;
use stacked_errors::Result;
use super_orchestrator::cli_docker::auto_exec;

/// Runs `super_orchestrator::docker_helpers::auto_exec`, `-it` is passed by
/// default
#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    /// Prefix of the name of the container
    #[arg(short, long)]
    prefix: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();
    auto_exec(["-it"], &args.prefix, ["sh"]).await?;
    Ok(())
}
