use core::fmt;
use std::{
    borrow::Cow,
    fmt::{Debug, Display},
    process::{ExitStatus, Stdio},
    str::Utf8Error,
    sync::Arc,
    time::Duration,
};

use log::warn;
use owo_colors::OwoColorize;
use stacked_errors::{DisplayStr, Error, Result, StackableErr};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{self, Child, ChildStdin},
    sync::Mutex,
    task::{self, JoinHandle},
    time::sleep,
};

use crate::{acquire_dir_path, next_terminal_color, FileOptions};

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
        f.write_fmt(format_args!(
            "Command {{\ncommand: {:?}\n, env_clear: {}, envs: {:?}, cwd: {:?}, stdout_log: {:?}, \
             stderr_log: {:?}, ci: {}, forget_on_drop: {}}}",
            DisplayStr(&self.get_unified_command()),
            self.env_clear,
            self.envs,
            self.cwd,
            self.stdout_log.as_ref().map(|x| &x.path),
            self.stderr_log.as_ref().map(|x| &x.path),
            self.ci,
            self.forget_on_drop
        ))
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
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
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

        // we purposely parenthesize in this way to avoid calling `panicking` in the
        // normal case
        if self.child_process.is_some() && (!std::thread::panicking()) {
            warn!(
                "A `CommandRunner` was dropped without being properly finished, the command was: \
                 {}",
                self.command
                    .as_ref()
                    .map(|c| c.get_unified_command())
                    .unwrap_or_default()
            )
        }
    }
}

#[must_use]
#[derive(Clone)]
pub struct CommandResult {
    // this information is kept around for failures
    pub command: Command,
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Debug for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandResult")
            .field("command", &self.command)
            .field("status", &self.status)
            .field("stdout", &DisplayStr(&self.stdout_as_utf8_lossy()))
            .field("stderr", &DisplayStr(&self.stderr_as_utf8_lossy()))
            .finish()
    }
}

impl Display for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Used for avoiding printing out lengthy stdouts
#[must_use]
#[derive(Debug, Clone)]
pub struct CommandResultNoDbg {
    pub command: Command,
    pub status: Option<ExitStatus>,
}

impl Display for CommandResultNoDbg {
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

    /// Sets `self.cwd`
    pub fn cwd(mut self, cwd: &str) -> Self {
        self.cwd = Some(cwd.to_owned());
        self
    }

    /// Adds an environment variable
    pub fn env(mut self, env_key: &str, env_val: &str) -> Self {
        self.envs.push((env_key.to_owned(), env_val.to_owned()));
        self
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

    pub(crate) fn get_unified_command(&self) -> String {
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
        command
    }

    pub async fn run_with_stdin<C: Into<Stdio>>(self, stdin_cfg: C) -> Result<CommandRunner> {
        let mut cmd = process::Command::new(&self.command);
        if self.env_clear {
            // must happen before the `envs` call
            cmd.env_clear();
        }
        if let Some(ref cwd) = self.cwd {
            let cwd = acquire_dir_path(cwd)
                .await
                .stack_err(|| format!("{self:?}.run()"))?;
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
            .stack_err(|| format!("{self:?}.run()"))?;
        let stdin = child.stdin.take();
        // TODO if we are going to do this we should allow getting active stdout from
        // the mutex
        let stdout = Arc::new(Mutex::new(vec![]));
        let stdout_arc_copy = Arc::clone(&stdout);
        let stderr = Arc::new(Mutex::new(vec![]));
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
        let terminal_color = if stdout_forward0.is_some() || stdout_forward1.is_some() {
            next_terminal_color()
        } else {
            owo_colors::AnsiColors::Default
        };
        // TODO have some kind of delay system that outputs after a delay if the line
        // has not been finished
        let mut stdout_read = BufReader::new(child.stdout.take().unwrap()).split(b'\n');
        let mut stderr_read = BufReader::new(child.stderr.take().unwrap()).split(b'\n');
        let command_name = self.command.clone();
        let child_id = child.id().unwrap();
        let mut handles: Vec<JoinHandle<()>> = vec![];
        handles.push(task::spawn(async move {
            loop {
                match stdout_read.next_segment().await {
                    Ok(Some(mut line)) => {
                        line.push(b'\n');
                        // copying for the `CommandResult`
                        stdout_arc_copy.lock().await.extend_from_slice(&line);
                        // copying to file
                        if let Some(ref mut stdout_file) = stdout_file {
                            stdout_file
                                .write_all(&line)
                                .await
                                .expect("command stdout to file copier failed");
                        }
                        let line_string = String::from_utf8_lossy(&line);
                        // forward stdout to stdout
                        if let Some(ref mut stdout_forward) = stdout_forward0 {
                            let s = format!("{} {}  |", command_name, child_id);
                            let _ = stdout_forward
                                .write(
                                    format!("{} {}", s.color(terminal_color), line_string)
                                        .as_bytes(),
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
                match stderr_read.next_segment().await {
                    Ok(Some(mut line)) => {
                        line.push(b'\n');
                        // copying for the `CommandResult`
                        stderr_arc_copy.lock().await.extend_from_slice(&line);
                        // copying to file
                        if let Some(ref mut stdout_file) = stderr_file {
                            stdout_file
                                .write_all(&line)
                                .await
                                .expect("command stderr to file copier failed");
                        }
                        // use the lossy version for
                        let line_string = String::from_utf8_lossy(&line);
                        // forward stderr to stdout
                        if let Some(ref mut stdout_forward) = stdout_forward1 {
                            let s = format!("{} {} E|", command_name, child_id);
                            let _ = stdout_forward
                                .write(
                                    format!("{} {}", s.color(terminal_color), line_string)
                                        .as_bytes(),
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
    pub async fn run(self) -> Result<CommandRunner> {
        self.run_with_stdin(Stdio::null()).await
    }

    pub async fn run_to_completion(self) -> Result<CommandResult> {
        self.run()
            .await
            .stack_err(|| "Command::run_to_completion")?
            .wait_with_output()
            .await
    }

    /// Same as [Command::run_to_completion] except it pipes `input` to the
    /// process stdin.
    pub async fn run_with_input_to_completion(self, input: &[u8]) -> Result<CommandResult> {
        let mut runner = self
            .run_with_stdin(Stdio::piped())
            .await
            .stack_err(|| "Command::run_with_input_to_completion")?;
        let mut stdin = runner
            .stdin
            .take()
            .stack_err(|| "using Stdio::piped() did not result in a stdin handle")?;
        stdin.write_all(input).await.stack_err(|| {
            "Command::run_with_input_to_completion() -> failed to write_all to process stdin"
        })?;
        // needs to close to actually finish
        drop(stdin);
        runner.wait_with_output().await
    }
}

impl CommandRunner {
    /// Attempts to force the command to exit, but does not wait for the request
    /// to take effect. This does not set `self.result`
    pub fn start_terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.start_kill().stack()
        } else {
            Ok(())
        }
    }

    /// Forces the command to exit. Drops the internal handle. Returns an error
    /// if some termination method has already been called (this will not
    /// error if the process exited itself, only if a termination function that
    /// removes the handle has been called).
    ///
    /// `self.result` is set, and `self.result.status` is set to `None`.
    pub async fn terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.kill().await.stack()?;
            drop(self.child_process.take().unwrap());
            let stdout = self.stdout.lock().await.clone();
            let stderr = self.stderr.lock().await.clone();
            self.result = Some(CommandResult {
                command: self.command.take().unwrap(),
                status: None,
                stdout,
                stderr,
            });
            Ok(())
        } else {
            Err(Error::from(
                "`CommandRunner` has already had some termination method called",
            ))
        }
    }

    /// Returns the `pid` of the child process. Returns `None` if the command
    /// has been terminated or the internal `id` call returned `None`.
    pub fn pid(&self) -> Option<u32> {
        if let Some(child_process) = self.child_process.as_ref() {
            if let Some(pid) = child_process.id() {
                return Some(pid)
            }
        }
        None
    }

    /// Sends a Unix `Signal` to the process.
    #[cfg(feature = "nix_support")]
    pub fn send_unix_signal(&self, unix_signal: nix::sys::signal::Signal) -> Result<()> {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(self.pid().stack()?).stack()?),
            unix_signal,
        )
        .stack()?;
        Ok(())
    }

    /// Has the same effect as "Ctrl-C" in a terminal. Users should preferably
    /// `wait_with_timeout` afterwards to wait for the process to exit
    /// correctly.
    #[cfg(feature = "nix_support")]
    pub fn send_unix_sigterm(&self) -> Result<()> {
        self.send_unix_signal(nix::sys::signal::Signal::SIGTERM)
    }

    // TODO for ridiculous output sizes, we may want something that only looks at
    // the exit status from `try_wait`, so keep the `_with_output` functions in case
    // we want a plain `wait` function

    async fn wait_with_output_internal(&mut self) -> Result<()> {
        let output = self
            .child_process
            .take()
            .stack_err(|| "`CommandRunner` has already had some termination method called")?
            .wait_with_output()
            .await
            .stack_err(|| format!("{self:?}.wait_with_output() -> failed when waiting on child"))?;
        while let Some(handle) = self.handles.pop() {
            handle
                .await
                .stack_err(|| format!("{self:?}.wait_with_output() -> `Command` task panicked"))?;
        }
        // note: the handles should be cleaned up first to make sure copies are finished
        // and no locks are being held
        let stdout = self.stdout.lock().await.clone();
        let stderr = self.stderr.lock().await.clone();
        self.result = Some(CommandResult {
            command: self.command.take().unwrap(),
            status: Some(output.status),
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
    pub async fn wait_with_output(mut self) -> Result<CommandResult> {
        self.wait_with_output_internal().await.stack()?;
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
            match self
                .child_process
                .as_mut()
                .stack_err(|| "`CommandRunner` has already had some termination method called")?
                .try_wait()
            {
                Ok(o) => {
                    if o.is_some() {
                        break
                    }
                }
                Err(e) => {
                    return Err(Error::from(e)).stack_err(|| {
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
    pub fn no_dbg(&self) -> CommandResultNoDbg {
        CommandResultNoDbg {
            command: self.command.clone(),
            status: self.status,
        }
    }

    pub fn successful(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            false
        }
    }

    pub fn successful_or_terminated(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            true
        }
    }

    pub fn assert_success(&self) -> Result<()> {
        if let Some(status) = self.status.as_ref() {
            if status.success() {
                Ok(())
            } else {
                Err(Error::from(format!(
                    "{self:#?}.assert_success() -> unsuccessful"
                )))
            }
        } else {
            Err(Error::from(format!(
                "{self:#?}.assert_success() -> termination was called before completion"
            )))
        }
    }

    /// Returns `str::from_utf8(&self.stdout)`
    pub fn stdout_as_utf8(&self) -> std::result::Result<&str, Utf8Error> {
        std::str::from_utf8(&self.stdout)
    }

    /// Returns `str::from_utf8(&self.stderr)`
    pub fn stderr_as_utf8(&self) -> std::result::Result<&str, Utf8Error> {
        std::str::from_utf8(&self.stderr)
    }

    /// Returns `String::from_utf8_lossy(&self.stdout)`
    pub fn stdout_as_utf8_lossy(&self) -> Cow<str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Returns `String::from_utf8_lossy(&self.stderr)`
    pub fn stderr_as_utf8_lossy(&self) -> Cow<str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

impl CommandResultNoDbg {
    pub fn successful(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            false
        }
    }

    pub fn successful_or_terminated(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            true
        }
    }

    pub fn assert_success(&self) -> Result<()> {
        if let Some(status) = self.status.as_ref() {
            if status.success() {
                Ok(())
            } else {
                Err(Error::from(format!(
                    "{self:#?}.assert_success() -> unsuccessful"
                )))
            }
        } else {
            Err(Error::from(format!(
                "{self:#?}.assert_success() -> termination was called before completion"
            )))
        }
    }
}
