use core::fmt;
use std::{
    fmt::{Debug, Display},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use log::warn;
use tokio::{
    fs::File,
    io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{self, Child},
    task,
    time::sleep,
};

use crate::{acquire_dir_path, acquire_file_path, Error, MapAddError, Result};

#[derive(Clone)]
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

impl Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut command = self.command.clone();
        if !self.args.is_empty() {
            command += " ";
            for (i, arg) in self.args.iter().enumerate() {
                command += arg;
                if i != (self.args.len() - 1) {
                    command += " ";
                }
            }
        }
        f.debug_struct("Command")
            .field("command", &command)
            .field("env_clear", &self.env_clear)
            .field("envs", &self.envs)
            .field("cwd", &self.cwd)
            .field("stdout_file", &self.stdout_file)
            .field("stderr_file", &self.stderr_file)
            .field("ci", &self.ci)
            .field("forget_on_drop", &self.forget_on_drop)
            .finish()
    }
}

#[derive(Debug)]
#[must_use]
pub struct CommandRunner {
    // this information is kept around for failures
    command: Option<Command>,
    // do not make public, some functions assume this is available
    child_process: Option<Child>,
    handles: Vec<tokio::task::JoinHandle<()>>,
    result: Option<CommandResult>,
}

impl Drop for CommandRunner {
    fn drop(&mut self) {
        // we could call `try_wait` and see if the process has actually exited or not,
        // but the user should have called one of the consuming functions
        if self.child_process.is_some() {
            warn!(
                "A `CommandRunner` was dropped and not properly finished, if not finished then \
                 the child process may continue using up resources or be force stopped at any \
                 time. The `Command` to run was: {:#?}",
                self.command
            )
        }
    }
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

impl Display for CommandResult {
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

    pub fn ci_mode(mut self, ci_mode: bool) -> Self {
        self.ci = ci_mode;
        self
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
            let cwd = acquire_dir_path(cwd)
                .await
                .map_add_err(|| format!("{self:?}.run()"))?;
            tmp.current_dir(cwd);
        }
        // do as much as possible before spawning the process
        let stdout_file = if let Some(ref path) = self.stdout_file {
            let path = acquire_file_path(path)
                .await
                .map_add_err(|| format!("{self:?}.run()"))?;
            Some(File::create(path).await?)
        } else {
            None
        };
        let mut child = tmp
            .args(&self.args)
            .envs(self.envs.iter().map(|x| (&x.0, &x.1)))
            .kill_on_drop(!self.forget_on_drop)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_add_err(|| format!("{self:?}.run()"))?;
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
                                .write(format!("{command} {child_id} stdout | {line}\n").as_bytes())
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
            command: Some(self),
            child_process: Some(child),
            handles,
            result: None,
        })
    }

    #[track_caller]
    pub async fn run_to_completion(self) -> Result<CommandResult> {
        self.run()
            .await
            .map_add_err(|| "Command::run_to_completion")?
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
            .map_add_err(|| ())
    }

    /// Forces the command to exit
    pub async fn terminate(&mut self) -> Result<()> {
        self.child_process
            .as_mut()
            .unwrap()
            .kill()
            .await
            .map_add_err(|| ())
    }

    // TODO for ridiculous output sizes, we may want something that only looks at
    // the exit status from `try_wait`, so keep the `_with_output` functions in case
    // we want a plain `wait` function

    #[track_caller]
    async fn wait_with_output_internal(&mut self) -> Result<()> {
        let output = self
            .child_process
            .take()
            .unwrap()
            .wait_with_output()
            .await
            .map_add_err(|| {
                format!("{self:?}.wait_with_output() -> failed when waiting on child",)
            })?;
        let stderr = String::from_utf8(output.stderr.clone()).map_add_err(|| {
            format!("{self:?}.wait_with_output() -> failed to parse stderr as utf8")
        })?;
        let stdout = String::from_utf8(output.stdout.clone()).map_add_err(|| {
            format!("{self:?}.wait_with_output() -> failed to parse stdout as utf8")
        })?;
        while let Some(handle) = self.handles.pop() {
            handle.await.map_add_err(|| {
                format!("{self:?}.wait_with_output() -> `Command` task panicked")
            })?;
        }
        self.result = Some(CommandResult {
            command: self.command.take().unwrap(),
            status: output.status,
            stdout,
            stderr,
        });
        Ok(())
    }

    /// Note: If this function succeeds, it only means that the OS calls and
    /// parsing all succeeded, it does not mean that the command itself had a
    /// successful return status, use `assert_status` or check the `status` on
    /// the `CommandResult`.
    #[track_caller]
    pub async fn wait_with_output(mut self) -> Result<CommandResult> {
        self.wait_with_output_internal().await?;
        Ok(self.result.take().unwrap())
    }

    pub async fn wait_with_timeout(&mut self, duration: Duration) -> Result<()> {
        // backoff control
        let mut interval = Duration::from_millis(1);
        let mut elapsed = Duration::ZERO;
        loop {
            match self.child_process.as_mut().unwrap().try_wait() {
                Ok(o) => {
                    if o.is_some() {
                        break
                    }
                }
                Err(e) => {
                    return e.map_add_err(|| {
                        "CommandRunner::wait_with_output_timeout failed at `try_wait` before \
                         reaching timeout or completed command"
                    })
                }
            }
            if elapsed > duration {
                return Err(Error::timeout())
            }
            sleep(interval).await;
            elapsed = elapsed.checked_add(interval).unwrap();
            // TODO is this a good default maximum interval?
            if interval < Duration::from_millis(128) {
                interval = interval.checked_mul(2).unwrap();
            }
        }
        self.wait_with_output_internal().await?;
        Ok(())
    }

    pub fn get_command_result(&mut self) -> Option<CommandResult> {
        self.result.take()
    }
}

impl CommandResult {
    #[track_caller]
    pub fn assert_success(&self) -> Result<()> {
        if self.status.success() {
            Ok(())
        } else {
            Err(Error::from(format!(
                "{self:#?}.check_status() -> unsuccessful"
            )))
        }
    }
}
