use clap::Parser;
use stacked_errors::Result;
use super_orchestrator::{ctrlc_init, docker_helpers::auto_exec_i, std_init};

/// Runs auto_exec_i
#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    /// Name of the container
    #[arg(short, long)]
    container_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;
    ctrlc_init()?;
    let args = Args::parse();
    auto_exec_i(&args.container_name).await?;
    Ok(())
}
