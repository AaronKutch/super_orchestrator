use clap::Parser;
use super_orchestrator::{docker_helpers::auto_exec_i, std_init, Result};

/// Runs auto_exec_i
#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long)]
    container_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;
    let args = Args::parse();
    auto_exec_i(&args.container_name).await?;
    Ok(())
}
