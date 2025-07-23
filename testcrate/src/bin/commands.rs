use std::{io::Write, iter, time::Duration};

use clap::Parser;
use stacked_errors::{ensure, ensure_eq, Result, StackableErr};
use super_orchestrator::{remove_files_in_dir, Command, FileOptions};
use tokio::time::sleep;
use tracing::info;

// this program calls itself to get stdout and stderr examples
#[derive(Parser, Debug)]
#[command(about)]
struct Args {
    #[arg(long)]
    print: bool,
    #[arg(long, default_value_t = String::new())]
    to_stdout: String,
    #[arg(long, default_value_t = String::new())]
    to_stderr: String,
    #[arg(long)]
    nonutf8: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = Args::parse();

    if args.nonutf8 {
        // testing that the recorder handles non-UTF8 correctly by converting to the
        // replacement symbol
        let mut bytes = vec![];
        bytes.extend("\u{1f60a}".as_bytes());
        for i in 0..=u8::MAX {
            bytes.push(i);
            if i.is_multiple_of(20) {
                bytes.push(b'\n');
            }
        }
        for i in 0..10 {
            bytes.extend("\u{1f60a}".as_bytes());
            if i % 3 == 0 {
                bytes.push(b'\n');
            }
        }
        bytes.push(b'\n');
        std::io::stdout()
            .write_all("(stdout)".as_bytes())
            .stack()
            .unwrap();
        std::io::stderr()
            .write_all("(stderr)".as_bytes())
            .stack()
            .unwrap();
        // check that starting with a cutoff multibyte char is continued
        let mut i = 5;
        for chunk in bytes.chunk_by(|_, _| {
            i += 1;
            i % 7 == 0
        }) {
            sleep(Duration::from_millis(10)).await;
            std::io::stdout().write_all(chunk).stack().unwrap();
            std::io::stdout().flush().unwrap();
        }
        for chunk in bytes.chunks(7) {
            std::io::stderr().write_all(chunk).stack().unwrap();
        }

        return Ok(());
    }

    if args.print {
        print!("{}", args.to_stdout);
        eprint!("{}", args.to_stderr);
        return Ok(());
    }

    remove_files_in_dir("./logs/", &["stdout.log", "stderr.log"])
        .await
        .stack()?;

    // testing edge cases around zero lengths and newlines
    test_copying(Some("hello".to_owned()), None).await.stack()?;
    test_copying(None, Some("world".to_owned())).await.stack()?;
    // note that the debug forwarders can outrun each other's ending newlines, won't
    // fix because it only effects debug and is only observable in a few programs
    test_copying(Some("hello".to_owned()), Some("world".to_owned()))
        .await
        .stack()?;
    test_copying(Some("".to_owned()), Some("".to_owned()))
        .await
        .stack()?;
    test_copying(Some("hello\n0".to_owned()), Some("world\n1".to_owned()))
        .await
        .stack()?;
    // insure that we are not affected by https://github.com/rust-lang/rust/issues/109907
    // (with respect to the records and log files)
    test_copying(
        Some("hello\n\n\n0\n".to_owned()),
        Some("world\n\n\n1\n".to_owned()),
    )
    .await
    .stack()?;

    // strange "filename or extension is too long" issue with Windows
    let many_bytes = if cfg!(windows) {
        String::from_iter(iter::repeat_n('e', 10 * 1024))
    } else {
        String::from_iter(iter::repeat_n('e', 105 * 1024))
    };

    // record and file size limiting, useful for some long running programs that may
    // end up with more output than there is memory.
    let comres = Command::new("cargo r --bin commands --quiet -- --print")
        .arg("--to-stdout")
        .arg(&many_bytes)
        .arg("--to-stderr")
        .arg(&many_bytes)
        .stdout_log(Some(FileOptions::write("./logs/stdout.log")))
        .stderr_log(Some(FileOptions::write("./logs/stderr.log")))
        .limit(Some(10 * 1024))
        .run_to_completion()
        .await
        .stack()?;
    comres.assert_success().stack()?;
    let expected = String::from_iter(iter::repeat_n('e', 10 * 1024));
    ensure_eq!(comres.stdout, expected.as_bytes());
    ensure_eq!(comres.stderr, expected.as_bytes());
    let file = FileOptions::read_to_string("./logs/stdout.log")
        .await
        .stack()?;
    ensure!(file.len() <= 10 * 1024);
    ensure!(!file.chars().any(|c| c != 'e'));
    let file = FileOptions::read_to_string("./logs/stderr.log")
        .await
        .stack()?;
    ensure!(file.len() <= 10 * 1024);
    ensure!(!file.chars().any(|c| c != 'e'));

    // disable recording entirely if we don't need it
    let comres = Command::new("cargo r --bin commands --quiet -- --print")
        .arg("--to-stdout")
        .arg(&many_bytes)
        .arg("--to-stderr")
        .arg(&many_bytes)
        .recording(false)
        .run_to_completion()
        .await
        .stack()?;
    comres.assert_success().stack()?;
    ensure!(comres.stdout.is_empty());
    ensure!(comres.stderr.is_empty());

    // check special handling for non-utf8 in stdout forwarding
    let comres = Command::new("cargo r --bin commands --quiet -- --nonutf8")
        .debug(true)
        .stdout_log(Some(FileOptions::write("./logs/stdout.log")))
        .stderr_log(Some(FileOptions::write("./logs/stderr.log")))
        .run_to_completion()
        .await
        .stack()?;
    ensure!(comres.stdout_as_utf8().is_err());
    ensure!(comres.stderr_as_utf8().is_err());
    dbg!(comres.stdout_as_utf8_lossy());
    dbg!(comres.stderr_as_utf8_lossy());
    dbg!(comres).assert_success().stack()?;

    // check command debug
    let command = Command::new("ls")
        .arg("-la")
        .env_clear(true)
        .envs([("TEST0", "test0"), ("TEST1", "test1")])
        .cwd("./")
        .recording(false)
        .stderr_debug(true)
        .stderr_log(Some(FileOptions::write("./hello.txt")))
        .record_limit(Some(9))
        .log_limit(Some(8))
        .forget_on_drop(true);
    dbg!(command);

    // check custom prefixes
    let command = Command::new("cargo r --bin commands -- --print --to-stdout hello")
        .debug(true)
        .stdout_debug_line_prefix(Some("stdout |".to_owned()))
        .stderr_debug_line_prefix(Some("stderr |".to_owned()));
    dbg!(&command);
    command
        .run_to_completion()
        .await
        .stack()?
        .assert_success()
        .stack()?;

    info!("test completed successfully");

    Ok(())
}

async fn test_copying(stdout: Option<String>, stderr: Option<String>) -> Result<()> {
    // pass these args recursively with the "--print" argument to get some example
    // standard streams
    let mut args = vec![];
    if let Some(ref stdout) = stdout {
        args.push("--to-stdout");
        args.push(stdout);
    }
    if let Some(ref stderr) = stderr {
        args.push("--to-stderr");
        args.push(stderr);
    }

    // Create and run the command. `--quiet` makes cargo not add any of its own
    // output, the arguments after the plain `--` are passed to the program run by
    // cargo (we could alternatively find a direct path to the compiled binary and
    // use that as the program directly, we are going through cargo to account for
    // the many possible placements of the binary depending on things like
    // `--release`).
    let comres = Command::new("cargo r --bin commands --quiet -- --print")
        .args(args)
        .debug(true)
        .stdout_log(Some(FileOptions::write("./logs/stdout.log")))
        .stderr_log(Some(FileOptions::write("./logs/stderr.log")))
        .run_to_completion()
        .await
        .stack()?;
    comres.assert_success().stack()?;

    // check that the records are as expected
    let expected_stdout = stdout.as_deref().map(|s| s.as_bytes()).unwrap_or_default();
    let expected_stderr = stderr.as_deref().map(|s| s.as_bytes()).unwrap_or_default();
    ensure_eq!(comres.stdout, expected_stdout);
    ensure_eq!(comres.stderr, expected_stderr);
    // check that the logs are as expected
    ensure_eq!(
        FileOptions::read_to_string("./logs/stdout.log")
            .await
            .stack()?
            .as_bytes(),
        expected_stdout
    );
    ensure_eq!(
        FileOptions::read_to_string("./logs/stderr.log")
            .await
            .stack()?
            .as_bytes(),
        expected_stderr
    );

    Ok(())
}
