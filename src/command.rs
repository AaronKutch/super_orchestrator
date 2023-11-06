use core::fmt;
use std::{
    borrow::{Borrow, Cow},
    collections::VecDeque,
    fmt::{Debug, Display},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    str::Utf8Error,
    sync::Arc,
    time::Duration,
};

use log::warn;
use owo_colors::AnsiColors;
use stacked_errors::{DisplayStr, Error, Result, StackableErr};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{self, Child, ChildStdin},
    sync::Mutex,
    task::{self, JoinHandle},
    time::{sleep, timeout},
};

use crate::{acquire_dir_path, next_terminal_color, FileOptions};

const DEFAULT_READ_LOOP_TIMEOUT: Duration = Duration::from_millis(300);

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
    pub cwd: Option<PathBuf>,
    /// Set to true by default, this enables recording of the `stdout` which can
    /// be accessed from `stdout_record` in the runner or `stdout` in the
    /// command result later
    pub stdout_recording: bool,
    /// Set to true by default, this enables recording of the `stderr` which can
    /// be accessed from `stderr_record` in the runner or `stderr` in the
    /// command result later
    pub stderr_recording: bool,
    /// If set, the command will copy the `stdout` to a file
    pub stdout_log: Option<FileOptions>,
    /// If set, the command will copy the `stderr` to a file
    pub stderr_log: Option<FileOptions>,
    /// Forward stdout to the current process stdout
    pub stdout_debug: bool,
    /// Forward stderr to the current process stderr
    pub stderr_debug: bool,
    /// Sets a limit on the number of bytes recorded by the stdout and stderr
    /// records separately, after which the records become circular buffers.
    /// This limits the potential memory used by a long running command. `None`
    /// means there is no limit.
    pub record_limit: Option<u64>,
    /// Sets a limit on the size of log files. Each time the limit is reached,
    /// the file is truncated.
    pub log_limit: Option<u64>,
    /// When recording the standard streams for a long running command, reading
    /// buffers should be paused periodically to copy data to records, debug,
    /// and log files, or else they will not update in real time and the task
    /// memory can increase without bound for cases that should be limited. This
    /// defaults to 300 ms.
    pub read_loop_timeout: Duration,
    /// If `false`, then killing the command on drop is enabled. NOTE: this
    /// being true or false should not be relied upon in normal program
    /// operation, `CommandRunner`s should be properly finished so that the
    /// child process is cleaned up properly.
    pub forget_on_drop: bool,
}

impl Default for Command {
    fn default() -> Self {
        Self {
            command: Default::default(),
            args: Default::default(),
            env_clear: Default::default(),
            envs: Default::default(),
            cwd: Default::default(),
            stderr_recording: true,
            stdout_recording: true,
            stdout_log: Default::default(),
            stderr_log: Default::default(),
            stdout_debug: Default::default(),
            stderr_debug: Default::default(),
            record_limit: Default::default(),
            log_limit: Default::default(),
            read_loop_timeout: DEFAULT_READ_LOOP_TIMEOUT,
            forget_on_drop: Default::default(),
        }
    }
}

impl Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "Command {{\ncommand: {:?}\n, env_clear: {}, envs: {:?}, cwd: {:?}, recording: ({}, \
             {}), log: ({:?}, {:?}), debug: ({}, {}), record_limit: {:?}, log_limit: {:?}, \
             forget_on_drop: {}}}",
            DisplayStr(&self.get_unified_command()),
            self.env_clear,
            self.envs,
            self.cwd,
            self.stdout_recording,
            self.stderr_recording,
            self.stdout_log.as_ref().map(|x| &x.path),
            self.stderr_log.as_ref().map(|x| &x.path),
            self.stdout_debug,
            self.stderr_debug,
            self.record_limit,
            self.log_limit,
            self.forget_on_drop
        ))
    }
}

/// Detached `Commands` are represented by this struct.
///
/// # Note
///
/// Locks on `stdout_record` and `stderr_record` should only be held long enough
/// to make a quick copy or other operation, because the task to record command
/// outputs needs the lock to progress.
///
/// If the `log` crate is used and an implementor is active, warnings from
/// bad `Drop`s can be issued
///
/// The `Default` impl is for if an empty runner not attached to anything is
/// needed for some reason.
#[must_use]
#[derive(Default)]
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
    pub stdout_record: Arc<Mutex<VecDeque<u8>>>,
    pub stderr_record: Arc<Mutex<VecDeque<u8>>>,
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

/// The result of a [Command](crate::Command)
#[must_use]
#[derive(Clone, Default)]
pub struct CommandResult {
    // the command information is kept around for failures
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
        f.write_fmt(format_args!("{:#?}", self))
    }
}

/// The same as a [CommandResult](crate::CommandResult), but the stdout and
/// stderr are not included in the debug info
#[must_use]
#[derive(Clone)]
pub struct CommandResultNoDebug {
    pub command: Command,
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Debug for CommandResultNoDebug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandResult")
            .field("command", &self.command)
            .field("status", &self.status)
            .finish()
    }
}

impl Display for CommandResultNoDebug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{:#?}", self))
    }
}

/// Used as the engine in the stdout and stderr recording tasks.
#[allow(clippy::too_many_arguments)]
async fn recorder<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    read_loop_timeout: Duration,
    mut std_read: BufReader<R>,
    mut std_record: Option<Arc<Mutex<VecDeque<u8>>>>,
    record_limit: Option<u64>,
    mut std_log: Option<File>,
    log_limit: Option<u64>,
    mut std_forward: Option<W>,
    command_name: String,
    child_id: u32,
    terminal_color: AnsiColors,
    std_err: bool,
) {
    // for tracking how much has been written to the filez
    let mut log_len = 0;
    let mut previous_newline = false;
    let mut nonempty = false;
    // 8 KB, like BufReader
    let mut buf = [0u8; 8 * 1024];
    loop {
        match timeout(read_loop_timeout, std_read.read(&mut buf)).await {
            Ok(Ok(bytes_read)) => {
                if bytes_read == 0 {
                    // if there has been nonempty output with no ending newline insert one upon
                    // completion
                    if nonempty && (!previous_newline) {
                        if let Some(ref mut stdout_forward) = std_forward {
                            let _ = stdout_forward
                                .write(b"\n")
                                .await
                                .expect("command recorder forwarding failed");
                            stdout_forward.flush().await.unwrap();
                        }
                    }
                    break
                }
                let bytes = &buf[..bytes_read];
                // copying to record
                if let Some(ref mut arc) = std_record {
                    let mut deque = arc.lock().await;
                    if let Some(limit) = record_limit {
                        let limit = usize::try_from(limit).unwrap();
                        if deque.len().saturating_add(bytes.len()) > limit {
                            if bytes.len() >= limit {
                                deque.clear();
                                deque.extend(bytes[(bytes.len() - limit)..].iter());
                            } else {
                                // use saturation because the record is public
                                let start = deque.len() + bytes.len() - limit;
                                deque.drain(start..);
                                deque.extend(bytes.iter());
                            }
                        } else {
                            deque.extend(bytes);
                        }
                    } else {
                        deque.extend(bytes);
                    }
                }
                // copying to file
                if let Some(ref mut std_log) = std_log {
                    log_len += bytes.len();
                    let mut reset = false;
                    if let Some(limit) = log_limit {
                        if (log_len as u64) > limit {
                            std_log.set_len(0).await.unwrap();
                            log_len = 0;
                            reset = true;
                        }
                    }
                    if !reset {
                        std_log
                            .write_all(bytes)
                            .await
                            .expect("command recorder to file failed");
                    }
                }
                // copying to std stream
                if let Some(ref mut std_forward) = std_forward {
                    // TODO handle cases where a utf8 codepoint is cut up, use from_utf8 and call
                    // valid_up_to on the Utf8Error. This is not critical, the records and logs need
                    // to be exact for arbitrary cases, the forwarding debug is for debug only.
                    let string = String::from_utf8_lossy(bytes).into_owned();
                    let mut first_iter = true;
                    for line in string.split_inclusive('\n') {
                        if (!first_iter) || previous_newline || (!nonempty) {
                            // need to format together otherwise stdout running into stderr is too
                            // common
                            let s = if std_err {
                                format!("{} {} E|", command_name, child_id)
                            } else {
                                format!("{} {}  |", command_name, child_id)
                            };
                            let _ = std_forward
                                .write(
                                    format!(
                                        "{} {}",
                                        owo_colors::OwoColorize::color(&s, terminal_color),
                                        line
                                    )
                                    .as_bytes(),
                                )
                                .await
                                .expect("command recorder forwarding failed");
                            first_iter = false;
                        } else {
                            let _ = std_forward
                                .write(line.as_bytes())
                                .await
                                .expect("command recorder forwarding failed");
                        }
                    }
                    std_forward.flush().await.unwrap();
                    previous_newline = string.bytes().last() == Some(b'\n');
                    nonempty = true;
                }
            }
            Ok(Err(e)) => {
                panic!("command recorder buffer read failed with {}", e)
            }
            // timeout
            Err(_) => (),
        }
    }
}

impl Command {
    /// Creates a `Command` that only sets the `command` and `args` and leaves
    /// other things as their default values. `cmd_with_args` is separated by
    /// whitespace, and the first part becomes the command the the others are
    /// extra prefixed args.
    ///
    /// In case an argument has spaces, it should be put into `args` as an
    /// unbroken `&str`. In case the command name has spaces, `self.command`
    /// can be changed directly.
    pub fn new(cmd_with_args: impl AsRef<str>, args: &[&str]) -> Self {
        let mut true_args = vec![];
        let mut command = String::new();
        for (i, part) in cmd_with_args.as_ref().split_whitespace().enumerate() {
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
            ..Default::default()
        }
    }

    /// Sets `self.cwd`
    pub fn cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = Some(cwd.as_ref().to_owned());
        self
    }

    /// Adds an argument
    pub fn arg(mut self, arg: impl AsRef<str>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Adds an environment variable
    pub fn env(mut self, env_key: impl AsRef<str>, env_val: impl AsRef<str>) -> Self {
        self.envs
            .push((env_key.as_ref().to_owned(), env_val.as_ref().to_owned()));
        self
    }

    /// Sets `stdout_debug` and `stderr_debug` for passing command standard
    /// streams to the standard streams of this process.
    pub fn debug(mut self, std_stream_debug: bool) -> Self {
        self.stdout_debug = std_stream_debug;
        self.stderr_debug = std_stream_debug;
        self
    }

    /// Sets `stdout_recording`
    pub fn stdout_recording(mut self, stdout_recording: bool) -> Self {
        self.stdout_recording = stdout_recording;
        self
    }

    /// Sets `stderr_recording`
    pub fn stderr_recording(mut self, stderr_recording: bool) -> Self {
        self.stderr_recording = stderr_recording;
        self
    }

    /// Sets `stdout_recording` and `stderr_recording`
    pub fn recording(mut self, recording: bool) -> Self {
        self.stdout_recording = recording;
        self.stderr_recording = recording;
        self
    }

    /// Sets `stdout_debug` for passing command stdout to the stdout of this
    /// process.
    pub fn stdout_debug(mut self, stdout_debug: bool) -> Self {
        self.stdout_debug = stdout_debug;
        self
    }

    /// Sets `stderr_debug` for passing command stderr to the stderr of this
    /// process.
    pub fn stderr_debug(mut self, stderr_debug: bool) -> Self {
        self.stderr_debug = stderr_debug;
        self
    }

    /// Sets `stdout_log` and `stderr_log` for copying command standard streams
    /// to the same file
    pub fn log<F: Borrow<FileOptions>>(mut self, std_stream_log: Option<F>) -> Self {
        if let Some(f) = std_stream_log {
            let f = f.borrow();
            self.stdout_log = Some(f.clone());
            self.stderr_log = Some(f.clone());
        }
        self
    }

    /// Sets `stdout_log` for copying command stdout to a file
    pub fn stdout_log<F: Borrow<FileOptions>>(mut self, stdout_log: Option<F>) -> Self {
        self.stdout_log = stdout_log.map(|f| f.borrow().clone());
        self
    }

    /// Sets `stderr_log` for copying command stderr to a file
    pub fn stderr_log<F: Borrow<FileOptions>>(mut self, stderr_log: Option<F>) -> Self {
        self.stderr_log = stderr_log.map(|f| f.borrow().clone());
        self
    }

    /// Sets `record_limit` for limiting stdout and stderr record byte lengths
    pub fn record_limit(mut self, record_limit: Option<u64>) -> Self {
        self.record_limit = record_limit;
        self
    }

    /// Sets `log_limit` for limiting stdout and stderr log file byte lengths
    pub fn log_limit(mut self, log_limit: Option<u64>) -> Self {
        self.log_limit = log_limit;
        self
    }

    /// Sets both `record_limit` and `log_limit`
    pub fn limit(mut self, limit: Option<u64>) -> Self {
        self.record_limit = limit;
        self.log_limit = limit;
        self
    }

    /// Sets `read_loop_timeout`
    pub fn read_loop_timeout(mut self, read_loop_timeout: Duration) -> Self {
        self.read_loop_timeout = read_loop_timeout;
        self
    }

    /// Gets the command and args interspersed with spaces
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

    /// Runs the command with a standard input, returning a `CommandRunner`
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
        let stdout_log = if let Some(ref options) = self.stdout_log {
            Some(options.acquire_file().await?)
        } else {
            None
        };
        let stderr_log = if let Some(ref options) = self.stderr_log {
            Some(options.acquire_file().await?)
        } else {
            None
        };
        // TODO if we are going to do this we should allow getting active stdout from
        // the mutex
        let stdout_record = Arc::new(Mutex::new(VecDeque::new()));
        let stdout_record_clone = if self.stdout_recording && (self.record_limit != Some(0)) {
            Some(Arc::clone(&stdout_record))
        } else {
            None
        };
        let stderr_record = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_record_clone = if self.stderr_recording && (self.record_limit != Some(0)) {
            Some(Arc::clone(&stderr_record))
        } else {
            None
        };
        let stdout_forward_stdout = if self.stdout_debug {
            Some(tokio::io::stdout())
        } else {
            None
        };
        let stderr_forward_stderr = if self.stderr_debug {
            Some(tokio::io::stderr())
        } else {
            None
        };
        let terminal_color = if stdout_forward_stdout.is_some() || stderr_forward_stderr.is_some() {
            next_terminal_color()
        } else {
            owo_colors::AnsiColors::Default
        };
        let record_limit = self.record_limit;
        let log_limit = self.log_limit;
        let command_name = self.command.clone();
        let read_loop_timeout = self.read_loop_timeout;
        let mut handles: Vec<JoinHandle<()>> = vec![];
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
        // TODO: If all recording is disabled do we drop the ChildStdout or do we need
        // to drop the output in a loop like we currently do?
        let stdout_read = BufReader::new(child.stdout.take().unwrap());
        let stderr_read = BufReader::new(child.stderr.take().unwrap());
        let child_id = child.id().unwrap();
        handles.push(task::spawn(recorder(
            read_loop_timeout,
            stdout_read,
            stdout_record_clone,
            record_limit,
            stdout_log,
            log_limit,
            stdout_forward_stdout,
            command_name,
            child_id,
            terminal_color,
            false,
        )));
        let command_name = self.command.clone();
        handles.push(task::spawn(recorder(
            read_loop_timeout,
            stderr_read,
            stderr_record_clone,
            record_limit,
            stderr_log,
            log_limit,
            stderr_forward_stderr,
            command_name,
            child_id,
            terminal_color,
            true,
        )));
        Ok(CommandRunner {
            command: Some(self),
            child_process: Some(child),
            handles,
            stdin,
            stdout_record,
            stderr_record,
            result: None,
        })
    }

    /// Calls [Command::run_with_stdin] with `Stdio::null()`
    pub async fn run(self) -> Result<CommandRunner> {
        self.run_with_stdin(Stdio::null()).await
    }

    /// Calls [Command::run] and waits for it to complete, returning the command
    /// result
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
    /// to take effect. This does not set `self.result`.
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
            let stdout = self.stdout_record.lock().await.iter().cloned().collect();
            let stderr = self.stderr_record.lock().await.iter().cloned().collect();
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
        let stdout = self.stdout_record.lock().await.iter().copied().collect();
        let stderr = self.stderr_record.lock().await.iter().copied().collect();
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
    /// Returns a `CommandResultNoDebug` version of `self`
    pub fn no_debug(self) -> CommandResultNoDebug {
        CommandResultNoDebug {
            command: self.command.clone(),
            status: self.status,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }

    /// Returns if the command completed (not terminated early) with a
    /// successful return status
    pub fn successful(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            false
        }
    }

    /// Returns if the command completed with a successful return status or was
    /// terminated early
    pub fn successful_or_terminated(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            true
        }
    }

    /// Returns a formatted error with relevant information if the command was
    /// not successful
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

impl CommandResultNoDebug {
    pub fn with_debug(self) -> CommandResult {
        CommandResult {
            command: self.command,
            status: self.status,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }

    /// Returns if the command completed (not terminated early) with a
    /// successful return status
    pub fn successful(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            false
        }
    }

    /// Returns if the command completed with a successful return status or was
    /// terminated early
    pub fn successful_or_terminated(&self) -> bool {
        if let Some(status) = self.status.as_ref() {
            status.success()
        } else {
            true
        }
    }

    /// Returns a formatted error with relevant information if the command was
    /// not successful
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
