use core::fmt;
use std::process::{ExitStatus, Stdio};

use tokio::{
    fs::File,
    io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{self, Child},
    task,
};

use crate::{acquire_dir_path, acquire_file_path, Error, Result};

#[derive(Debug, Clone)]
pub struct Command {
    pub command: String,
    pub args: Vec<String>,
    /// Clears the environment variable map before applying `envs`
    pub env_clear: bool,
    /// Environment variable mappings
    pub envs: Vec<(String, String)>,
    /// Working directory for process
    pub cwd: Option<String>,
    pub stdout_file: Option<String>,
    pub stderr_file: Option<String>,
    /// Override to forward stdouts and stderrs to the current processes stdout
    /// and stderr
    pub ci: bool,
    /// If `false`, then `kill_on_drop` is enabled. NOTE: this being true or
    /// false should not be relied upon in normal program operation, `Commands`
    /// should be properly consumed by a method taking `self` so that the child
    /// process is cleaned up properly.
    pub forget_on_drop: bool,
}

#[derive(Debug)]
#[must_use]
pub struct CommandRunner {
    // this information is kept around for failures
    pub command: Command,
    // do not make public, some functions assume this is available
    child_process: Option<Child>,
    pub handles: Vec<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
#[must_use]
pub struct CommandResult {
    // this information is kept around for failures
    pub command: Command,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl fmt::Display for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Command {
    /// Creates a `Command` that only sets the `command` and `args` and leaves
    /// other things as their default values.
    pub fn new(command: &str, args: &[&str]) -> Self {
        Self {
            command: command.to_owned(),
            args: args
                .iter()
                .fold(Vec::with_capacity(args.len()), |mut acc, e| {
                    acc.push(e.to_string());
                    acc
                }),
            env_clear: false,
            envs: vec![],
            cwd: None,
            stdout_file: None,
            stderr_file: None,
            ci: false,
            forget_on_drop: false,
        }
    }

    #[track_caller]
    pub async fn run(self) -> Result<CommandRunner> {
        let mut tmp = process::Command::new(&self.command);
        if self.env_clear {
            // must happen before the `envs` call
            tmp.env_clear();
        }
        if let Some(ref cwd) = self.cwd {
            // TODO when `track_caller` works on `async`, we might be able to remove some of
            // these `locate`s
            let cwd = match acquire_dir_path(cwd).await {
                Ok(o) => o,
                Err(e) => return Err(e.locate().generic_error(&format!("{self:?}.run() -> "))),
            };
            tmp.current_dir(cwd);
        }
        // do as much as possible before spawning the process
        let stdout_file = if let Some(ref path) = self.stdout_file {
            let path = match acquire_file_path(path).await {
                Ok(o) => o,
                Err(e) => return Err(e.locate().generic_error(&format!("{self:?}.run() -> "))),
            };
            Some(File::create(path).await?)
        } else {
            None
        };
        let child_res = tmp
            .args(&self.args)
            .envs(self.envs.iter().map(|x| (&x.0, &x.1)))
            .kill_on_drop(!self.forget_on_drop)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        match child_res {
            Ok(mut child) => {
                let child_id = child.id().unwrap_or(0);
                let command = self.command.clone();
                let mut handles = vec![];
                // note: only one thing can take `child.stdout`
                if self.ci {
                    let stdout = child.stdout.take().unwrap();
                    // in CI mode print to stdout
                    let mut lines = BufReader::new(stdout).lines();
                    let mut writer = BufWriter::new(tokio::io::stdout());
                    handles.push(task::spawn(async move {
                        loop {
                            match lines.next_line().await {
                                Ok(Some(line)) => {
                                    let _ = writer
                                        .write(
                                            format!("{command} {child_id} stdout | {line}\n")
                                                .as_bytes(),
                                        )
                                        .await
                                        .unwrap();
                                    writer.flush().await.unwrap();
                                }
                                Ok(None) => break,
                                Err(e) => panic!("stdout line copier failed with {}", e),
                            }
                        }
                    }));
                } else if let Some(mut stdout_file) = stdout_file {
                    let mut stdout = child.stdout.take().unwrap();
                    handles.push(task::spawn(async move {
                        io::copy(&mut stdout, &mut stdout_file)
                            .await
                            .expect("stdout copier failed");
                    }));
                }
                Ok(CommandRunner {
                    command: self,
                    child_process: Some(child),
                    handles,
                })
            }
            Err(e) => Err(Error::from(e)
                .locate()
                .generic_error(&format!("{self:?}.run() -> "))),
        }
    }

    #[track_caller]
    pub async fn run_to_completion(self) -> Result<CommandResult> {
        self.run()
            .await
            .map_err(|e| e.generic_error("Command::run_to_completion -> "))?
            .wait_with_output()
            .await
    }
}

impl CommandRunner {
    /// Attempts to force the command to exit, but does not wait for the request
    /// to take effect.
    pub fn start_terminate(&mut self) -> Result<()> {
        self.child_process
            .as_mut()
            .unwrap()
            .start_kill()
            .map_err(From::from)
    }

    /// Forces the command to exit
    pub async fn terminate(&mut self) -> Result<()> {
        self.child_process
            .as_mut()
            .unwrap()
            .kill()
            .await
            .map_err(From::from)
    }

    /// Note: If this function succeeds, it only means that the OS calls and
    /// parsing all succeeded, it does not mean that the command itself had a
    /// successful return status, use `assert_status` or check the `status` on
    /// the `CommandResult`.
    #[track_caller]
    pub async fn wait_with_output(mut self) -> Result<CommandResult> {
        let output = match self.child_process.take().unwrap().wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                return Err(Error::from(format!(
                    "{self:?}.wait_with_output() -> failed when waiting on child: {}",
                    e
                )))
            }
        };
        let stderr = if let Ok(stderr) = String::from_utf8(output.stderr.clone()) {
            stderr
        } else {
            return Err(Error::from(format!(
                "{self:?}.wait_with_output() -> failed to parse stderr as utf8"
            )))
        };
        let stdout = if let Ok(stdout) = String::from_utf8(output.stdout.clone()) {
            stdout
        } else {
            return Err(Error::from(format!(
                "{self:?}.wait_with_output() -> failed to parse stdout as utf8"
            )))
        };
        while let Some(handle) = self.handles.pop() {
            match handle.await {
                Ok(()) => (),
                Err(e) => {
                    return Err(Error::from(format!(
                        "{self:?}.wait_with_output() -> `Command` task panicked: {e}"
                    )))
                }
            }
        }
        Ok(CommandResult {
            command: self.command,
            status: output.status,
            stdout,
            stderr,
        })
    }
}

impl CommandResult {
    #[track_caller]
    pub fn assert_success(&self) -> Result<()> {
        if self.status.success() {
            Ok(())
        } else {
            Err(Error::from("{self:?}.check_status() -> unsuccessful"))
        }
    }
}
