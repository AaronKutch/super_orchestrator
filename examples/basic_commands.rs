use stacked_errors::{ensure, StackableErr};
use super_orchestrator::{stacked_errors::Result, Command, CommandResult, CommandResultNoDbg};

#[tokio::main]
async fn main() -> Result<()> {
    println!("example 0\n");

    // this runs the "ls" command just like how it would if run from command line
    // from the same directory
    let comres: CommandResult = Command::new("ls", &[]).run_to_completion().await.stack()?;
    // The result from the `run_to_completion` command only returns if OS calls or
    // other infrastructure failed. The status of the `CommandResult` needs to be
    // checked to see if the return status of the command was actually ok or not.
    // `assert_success` just checks the `status` and returns a nicely formatted
    // error if the status is not a success status.
    comres.assert_success().stack()?;

    // access all the public fields
    dbg!(
        &comres.command,
        &comres.status,
        &comres.stdout.len(),
        &comres.stderr.len()
    );
    // helper methods
    ensure!(comres.successful());
    ensure!(comres.successful_or_terminated());
    println!("stdout:\n{}", comres.stdout_as_utf8().stack()?);
    println!("stderr:\n{}", comres.stderr_as_utf8().stack()?);
    println!("display:\n{comres}");
    println!("debug:\n{comres:?}");
    println!("pretty print:\n{comres:#?}");

    // with some commands with a huge output, we may not want the std streams in the
    // debug or display outputs
    let comres: CommandResultNoDbg = comres.no_dbg();
    // these will not have the std streams in their output, only command and status
    // information
    comres.assert_success().stack()?;
    println!("debug:\n{comres:?}");
    println!("pretty print:\n{comres:#?}");
    println!("display:\n{comres}");

    println!("\n\nexample 1\n");

    // debug mode forwards the standard streams of the command to the current
    // process
    let comres: CommandResult = Command::new("ls", &[])
        .debug(true)
        .run_to_completion()
        .await
        .stack()?;
    comres.assert_success().stack()?;

    Ok(())
}
