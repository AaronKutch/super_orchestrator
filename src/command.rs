use core::fmt;
use std::{
    fmt::{Debug, Display},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use log::warn;
use stacked_errors::{Error, MapAddError, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{self, Child, ChildStdin},
    sync::Mutex,
    task::{self, JoinHandle},
    time::sleep,
};

use crate::{acquire_dir_path, DisplayStr, FileOptions};

/// An OS Command, this is `tokio::process::Command` wrapped in a bunch of
/// helping functionality.
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
    /// If set, the command will copy the `stdout` to a file
    pub stdout_log: Option<FileOptions>,
    /// If set, the command will copy the `stderr` to a file
    pub stderr_log: Option<FileOptions>,
    /// Forward stdouts and stderrs to the current processes stdout and stderr
    pub ci: bool,
    /// If `false`, then `kill_on_drop` is enabled. NOTE: this being true or
    /// false should not be relied upon in normal program operation,
    /// `CommandRunner`s should be properly finished so that the child
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
            .field("command", &DisplayStr(&command))
            .field("env_clear", &self.env_clear)
            .field("envs", &self.envs)
            .field("cwd", &self.cwd)
            .field("stdout_log", &self.stdout_log)
            .field("stderr_log", &self.stderr_log)
            .field("ci", &self.ci)
            .field("forget_on_drop", &self.forget_on_drop)
            .finish()
    }
}

/// Note: if the `log` crate is used and an implementor active, warnings from
/// bad `Drop`s can be issued
#[must_use]
pub struct CommandRunner {
    // this information is kept around for failures
    /// The command this runner was started with
    command: Option<Command>,
    // do not make public, some functions assume this is available
    child_process: Option<Child>,
    handles: Vec<tokio::task::JoinHandle<()>>,
    stdin: Option<ChildStdin>,
    // If we take out the `ChildStderr` from the process, the results will have nothing in them. If
    // we are actively copying stdout/stderr to a file and/or forwarding stdout, we need to also be
    // copying it to here in order to not lose the data.
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    result: Option<CommandResult>,
}

impl Debug for CommandRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // don't try to display `stdout` and `stderr`, leave that for the result
        f.debug_struct("CommandRunner")
            .field("command", &self.command)
            .field("child_process", &self.child_process)
            .field("handles", &self.handles)
            .field("result", &self.result)
            .finish()
    }
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

#[must_use]
pub struct CommandResult {
    // this information is kept around for failures
    pub command: Command,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Debug for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandResult")
            .field("command", &self.command)
            .field("status", &self.status)
            .field("stdout", &DisplayStr(&self.stdout))
            .field("stderr", &DisplayStr(&self.stderr))
            .finish()
    }
}

impl Display for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Command {
    /// Creates a `Command` that only sets the `command` and `args` and leaves
    /// other things as their default values. `cmd_with_args` is separated by
    /// whitespace, and the first part becomes the command the the others are
    /// extra prefixed args.
    pub fn new(cmd_with_args: &str, args: &[&str]) -> Self {
        let mut true_args = vec![];
        let mut command = String::new();
        for (i, part) in cmd_with_args.split_whitespace().enumerate() {
            if i == 0 {
                command = part.to_owned();
            } else {
                true_args.push(part.to_owned());
            }
        }
        for remaining_arg in args {
            true_args.push((*remaining_arg).to_owned());
        }
        Self {
            command,
            args: true_args,
            env_clear: false,
            envs: vec![],
            cwd: None,
            stdout_log: None,
            stderr_log: None,
            ci: false,
            forget_on_drop: false,
        }
    }

    pub fn ci_mode(mut self, ci_mode: bool) -> Self {
        self.ci = ci_mode;
        self
    }

    pub fn stdout_log(mut self, log_file_options: &FileOptions) -> Self {
        self.stdout_log = Some(log_file_options.clone());
        self
    }

    pub fn stderr_log(mut self, log_file_options: &FileOptions) -> Self {
        self.stderr_log = Some(log_file_options.clone());
        self
    }

    #[track_caller]
    pub async fn run_with_stdin<C: Into<Stdio>>(self, stdin_cfg: C) -> Result<CommandRunner> {
        let mut cmd = process::Command::new(&self.command);
        if self.env_clear {
            // must happen before the `envs` call
            cmd.env_clear();
        }
        if let Some(ref cwd) = self.cwd {
            let cwd = acquire_dir_path(cwd)
                .await
                .map_add_err(|| format!("{self:?}.run()"))?;
            cmd.current_dir(cwd);
        }
        // do as much as possible before spawning the process
        let mut stdout_file = if let Some(ref log_file_options) = self.stdout_log {
            Some(log_file_options.acquire_file().await?)
        } else {
            None
        };
        let mut stderr_file = if let Some(ref log_file_options) = self.stderr_log {
            Some(log_file_options.acquire_file().await?)
        } else {
            None
        };
        cmd.args(&self.args)
            .envs(self.envs.iter().map(|x| (&x.0, &x.1)))
            .kill_on_drop(!self.forget_on_drop);
        let mut child = cmd
            .stdin(stdin_cfg)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_add_err(|| format!("{self:?}.run()"))?;
        let stdin = child.stdin.take();
        // TODO if we are going to do this we should allow getting active stdout from
        // the mutex
        let stdout = Arc::new(Mutex::new(String::new()));
        let stdout_arc_copy = Arc::clone(&stdout);
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_arc_copy = Arc::clone(&stderr);
        let mut stdout_forward0 = if self.ci {
            Some(tokio::io::stdout())
        } else {
            None
        };
        let mut stdout_forward1 = if self.ci {
            Some(tokio::io::stdout())
        } else {
            None
        };
        let mut stdout_read = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut stderr_read = BufReader::new(child.stderr.take().unwrap()).lines();
        let command_name = self.command.clone();
        let child_id = child.id().unwrap();
        let mut handles: Vec<JoinHandle<()>> = vec![];
        handles.push(task::spawn(async move {
            loop {
                match stdout_read.next_line().await {
                    Ok(Some(mut line)) => {
                        line.push('\n');
                        // copying for the `CommandResult`
                        stdout_arc_copy.lock().await.push_str(&line);
                        // copying to file
                        if let Some(ref mut stdout_file) = stdout_file {
                            stdout_file
                                .write_all(line.as_bytes())
                                .await
                                .expect("command stdout to file copier failed");
                        }
                        // forward stdout to stdout
                        if let Some(ref mut stdout_forward) = stdout_forward0 {
                            let _ = stdout_forward
                                .write(
                                    format!("{command_name} {child_id} stdout | {line}").as_bytes(),
                                )
                                .await
                                .expect("command stdout to stdout copier failed");
                            stdout_forward.flush().await.unwrap();
                        }
                    }
                    Ok(None) => break,
                    Err(e) => panic!("command stdout line copier failed with {}", e),
                }
            }
        }));
        let command_name = self.command.clone();
        handles.push(task::spawn(async move {
            loop {
                match stderr_read.next_line().await {
                    Ok(Some(mut line)) => {
                        line.push('\n');
                        // copying for the `CommandResult`
                        stderr_arc_copy.lock().await.push_str(&line);
                        // copying to file
                        if let Some(ref mut stdout_file) = stderr_file {
                            stdout_file
                                .write_all(line.as_bytes())
                                .await
                                .expect("command stderr to file copier failed");
                        }
                        // forward stderr to stdout
                        if let Some(ref mut stdout_forward) = stdout_forward1 {
                            let _ = stdout_forward
                                .write(
                                    format!("{command_name} {child_id} stderr | {line}").as_bytes(),
                                )
                                .await
                                .expect("command stderr to stdout copier failed");
                            stdout_forward.flush().await.unwrap();
                        }
                    }
                    Ok(None) => break,
                    Err(e) => panic!("command stderr line copier failed with {}", e),
                }
            }
        }));
        Ok(CommandRunner {
            command: Some(self),
            child_process: Some(child),
            handles,
            stdin,
            stdout,
            stderr,
            result: None,
        })
    }

    /// Calls [Command::run_with_stdin] with `Stdio::null()`
    #[track_caller]
    pub async fn run(self) -> Result<CommandRunner> {
        self.run_with_stdin(Stdio::null()).await
    }

    #[track_caller]
    pub async fn run_to_completion(self) -> Result<CommandResult> {
        self.run()
            .await
            .map_add_err(|| "Command::run_to_completion")?
            .wait_with_output()
            .await
    }

    /// Same as [Command::run_to_completion] except it pipes `input` to the
    /// process stdin.
    #[track_caller]
    pub async fn run_with_input_to_completion(self, input: &[u8]) -> Result<CommandResult> {
        let mut runner = self
            .run_with_stdin(Stdio::piped())
            .await
            .map_add_err(|| "Command::run_with_input_to_completion")?;
        let mut stdin = runner
            .stdin
            .take()
            .map_add_err(|| "using Stdio::piped() did not result in a stdin handle")?;
        stdin.write_all(input).await.map_add_err(|| {
            "Command::run_with_input_to_completion() -> failed to write_all to process stdin"
        })?;
        // needs to close to actually finish
        drop(stdin);
        runner.wait_with_output().await
    }
}

impl CommandRunner {
    /// Attempts to force the command to exit, but does not wait for the request
    /// to take effect.
    pub fn start_terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.start_kill().map_add_err(|| ())
        } else {
            Ok(())
        }
    }

    /// Forces the command to exit
    pub async fn terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.kill().await.map_add_err(|| ())?;
            self.child_process.take().unwrap();
        }
        Ok(())
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
        /*let stderr = String::from_utf8(output.stderr.clone()).map_add_err(|| {
            format!("{self:?}.wait_with_output() -> failed to parse stderr as utf8")
        })?;
        let stdout = String::from_utf8(output.stdout.clone()).map_add_err(|| {
            format!("{self:?}.wait_with_output() -> failed to parse stdout as utf8")
        })?;*/
        while let Some(handle) = self.handles.pop() {
            handle.await.map_add_err(|| {
                format!("{self:?}.wait_with_output() -> `Command` task panicked")
            })?;
        }
        // note: the handles should be cleaned up first to make sure copies are finished
        // and no locks are being held
        let stdout = self.stdout.lock().await.clone();
        let stderr = self.stderr.lock().await.clone();
        self.result = Some(CommandResult {
            command: self.command.take().unwrap(),
            status: output.status,
            stdout,
            stderr,
        });
        Ok(())
    }

    /// Finishes the `CommandResult` (or stalls forever if the OS command does,
    /// use `wait_with_timeout` for a timeout). Note: If this function
    /// succeeds, it only means that the OS calls and parsing all succeeded,
    /// it does not mean that the command itself had a successful return
    /// status, use `assert_status` or check the `status` on
    /// the `CommandResult`.
    #[track_caller]
    pub async fn wait_with_output(mut self) -> Result<CommandResult> {
        self.wait_with_output_internal().await?;
        Ok(self.result.take().unwrap())
    }

    /// If the command does not complete after `duration`, returns a timeout
    /// error. After `Ok(())` is returned, the `CommandRunner` is finished and
    /// you can call `get_command_result`. Call [Error::is_timeout()] on the
    /// error to see if it was a timeout or another kind of error.
    ///
    /// Note: use `Duration::ZERO` if you want a single attempt
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
                        "CommandRunner::wait_with_timeout failed at `try_wait` before reaching \
                         timeout or completed command"
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

    /// Assuming that `self` is finished after
    /// [CommandRunner::wait_with_timeout], this can be called
    pub fn get_command_result(mut self) -> Option<CommandResult> {
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
