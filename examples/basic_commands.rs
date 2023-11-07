use std::time::Duration;

use stacked_errors::{ensure, ensure_eq, StackableErr};
use super_orchestrator::{
    sh, stacked_errors::Result, std_init, Command, CommandResult, CommandResultNoDebug, FileOptions,
};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    // logging to detect bad drops
    std_init()?;

    println!("example 0\n");

    // this runs the "ls" command just like how it would if run from command line
    // from the same directory
    let comres: CommandResult = Command::new("ls").run_to_completion().await.stack()?;
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
    let comres: CommandResultNoDebug = comres.no_debug();
    // these will not have the std streams in their output, only command and status
    // information
    comres.assert_success().stack()?;
    println!("debug:\n{comres:?}");
    println!("pretty print:\n{comres:#?}");
    println!("display:\n{comres}");

    println!("\n\nexample 1\n");

    // debug mode forwards the standard streams of the command to the current
    // process
    let comres = Command::new("ls")
        .debug(true)
        .run_to_completion()
        .await
        .stack()?;
    comres.assert_success().stack()?;

    // shorthand for the above
    sh(["ls"]).await.stack()?;

    // add an argument to the command this is the same as `ls ./example` on a
    // command line
    sh(["ls", "./examples"]).await.stack()?;

    // `super_orchestrator::Command::new` and the first iterator element of
    // `super_orchestrator::sh` have the feature that they are split by whitespace,
    // using the first segment for the command, and prefixes the others as separate
    // arguments
    sh(["ls ./examples"]).await.stack()?;

    // Note: when trying to access the file "filename with spaces.txt", you would
    // type on a command line `ls "filename with spaces"`. However, it would not
    // mean the same thing to use

    //sh(["ls \"filename with spaces\""])
    // or
    //sh(["ls", "filename", "with", "spaces"])
    // or
    //sh["ls", "\"filename with spaces\""])

    // because the quotation marks are for the commandline, a signal, to pass
    // "filename with spaces" as a single OS argument without the literal quotation
    // marks. The correct way is:

    //sh(["ls", "filename with spaces.txt"]).await.stack()?;
    // or
    //Command::new("ls").arg("filename with spaces.txt") ...

    // accounting for the right relative directory it is
    sh(["ls", "examples/filename with spaces.txt"])
        .await
        .stack()?;

    // This triggers the command to have an unsuccessful exit status.
    // Debug stderr lines have an 'E' in them to distinguish from stdout lines,
    ensure!(sh(["ls ./nonexistent"]).await.is_err());

    // there is not an error at the command running stage
    let comres = Command::new("ls ./nonexistent")
        .run_to_completion()
        .await
        .stack()?;
    // but rather at this stage
    ensure!(comres.assert_success().is_err());

    println!("\n\nexample 2\n");

    // in the case of long running programs that we want to detach to the
    // background, we can use `run`
    let mut ls_runner = Command::new("sleep 1").debug(true).run().await.stack()?;
    // we can do this on Linux to emulate a Ctrl+C from commandline
    //ls_runner.send_unix_sigterm()
    // do this to go back to blocking like `run_to_completion` does
    //ls_runner.wait_with_output();
    // do this to be able to write poll loops
    loop {
        match ls_runner
            .wait_with_timeout(Duration::from_millis(200))
            .await
        {
            Ok(()) => break,
            Err(e) => {
                if e.is_timeout() {
                    dbg!()
                } else {
                    e.stack()?;
                }
            }
        }
    }
    // use this once after a termination function is successful
    let comres = ls_runner.get_command_result().unwrap();
    comres.assert_success().stack()?;

    // also note that for very long running commands, you may want to set
    // `record_limit` and `log_limit`, or disable recording and logging altogether

    println!("\n\nexample 3\n");

    // changing the current working directory of the command
    let comres = Command::new("ls")
        .debug(true)
        .cwd("./examples")
        .run_to_completion()
        .await
        .stack()?;
    comres.assert_success().stack()?;

    // Sending output to a file, debugging, and using the records simultaneously.
    // This is the main utility of the `super_orchestrator` `Command` struct v.s.
    // many others for which you can only do one at a time for a long running
    // program. Note that `FileOptions::write` creates and truncates by default, but
    // this can be changed.
    let ls_runner = Command::new("ls")
        .debug(true)
        .stdout_log(Some(FileOptions::write(
            "./logs/basic_commands_stdout_ex.log",
        )))
        .stderr_log(Some(FileOptions::write(
            "./logs/basic_commands_stderr_ex.log",
        )))
        .run()
        .await
        .stack()?;
    sleep(Duration::from_millis(10)).await;
    let record = ls_runner.stdout_record.lock().await;
    let len = record.len();
    // drop mutex guards immediately after using them
    drop(record);
    let comres = ls_runner.wait_with_output().await.stack()?;
    comres.assert_success().stack()?;
    ensure_eq!(
        FileOptions::read_to_string("./logs/basic_commands_stdout_ex.log")
            .await
            .stack()?
            .len(),
        len
    );

    println!("\n\nexample 4\n");

    if Command::new("grep").run_to_completion().await.is_err() {
        println!("grep not found, last example cannot be run");
        return Ok(())
    }

    // Now suppose we want to pipe input to the "echo" command. The `echo
    // "hello\nworld" | grep h` line that would be typed into a commandline also has
    // special interpreting that is equivalent to:

    let comres = Command::new("grep h")
        .debug(true)
        .run_with_input_to_completion(b"hello\nworld")
        .await
        .stack()?;
    comres.assert_success().stack()?;
    ensure_eq!(comres.stdout_as_utf8().unwrap(), "hello\n");

    Ok(())
}
