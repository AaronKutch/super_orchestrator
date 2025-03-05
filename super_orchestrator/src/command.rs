use core::fmt;
use std::{
    borrow::{Borrow, Cow},
    ffi::{OsStr, OsString},
    fmt::{Debug, Display},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    str::Utf8Error,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use stacked_errors::{bail_locationless, DisplayStr, Result, StackableErr};
use tokio::io::AsyncWriteExt;

use crate::{command_runner, CommandRunner, FileOptions};

const DEFAULT_READ_LOOP_TIMEOUT: Duration = Duration::from_millis(300);

/// An OS Command, this is `tokio::process::Command` wrapped in a bunch of
/// helping functionality.
#[derive(Clone, Serialize, Deserialize)]
pub struct Command {
    /// The program to run.
    pub program: OsString,
    /// All the arguments that will be passed to the program
    pub args: Vec<OsString>,
    /// If set, the environment variable map is cleared (before the `envs` are
    /// applied)
    pub env_clear: bool,
    /// Environment variable mappings
    pub envs: Vec<(OsString, OsString)>,
    /// Working directory for process. `acquire_dir_path` is used on this in the
    /// functions that run the `Commanmd`.
    pub cwd: Option<PathBuf>,
    /// Set to true by default, this enables recording of the `stdout` which can
    /// be accessed from `stdout_record` in the runner or `stdout` in the
    /// command result later
    pub stdout_recording: bool,
    /// Set to true by default, this enables recording of the `stderr` which can
    /// be accessed from `stderr_record` in the runner or `stderr` in the
    /// command result later
    pub stderr_recording: bool,
    /// If set, the command will copy the `stdout` to the file
    pub stdout_log: Option<FileOptions>,
    /// If set, the command will copy the `stderr` to the file
    pub stderr_log: Option<FileOptions>,
    /// Forward stdout to the current process stdout
    pub stdout_debug: bool,
    /// Forward stderr to the current process stderr
    pub stderr_debug: bool,
    /// If the default stdout debug line prefix should be overridden
    pub stdout_debug_line_prefix: Option<String>,
    /// If the default stderr debug line prefix should be overridden
    pub stderr_debug_line_prefix: Option<String>,
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
            program: Default::default(),
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
            stdout_debug_line_prefix: None,
            stderr_debug_line_prefix: None,
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
            "Command {{\nprogram: {:?}\n,",
            DisplayStr(&self.get_unified_command()),
        ))?;
        if self.env_clear {
            f.write_fmt(format_args!(" env_clear: true,",))?;
        }
        if !self.envs.is_empty() {
            f.write_fmt(format_args!(" envs: {:?},", self.envs))?;
        }
        if let Some(cwd) = &self.cwd {
            f.write_fmt(format_args!(" cwd: {cwd:?},"))?;
        }
        // potential accident cases
        if !(self.stdout_recording && self.stderr_recording) {
            f.write_fmt(format_args!(
                " recording: ({}, {}),",
                self.stdout_recording, self.stderr_recording
            ))?;
        }
        if let Some(log) = self.stdout_log.as_ref().map(|x| &x.path) {
            f.write_fmt(format_args!(" stdout_log: {log:?},"))?;
        }
        if let Some(log) = self.stderr_log.as_ref().map(|x| &x.path) {
            f.write_fmt(format_args!(" stderr_log: {log:?},"))?;
        }
        if self.stdout_debug || self.stderr_debug {
            f.write_fmt(format_args!(
                " debug: ({}, {}),",
                self.stdout_debug, self.stderr_debug
            ))?;
        }
        if let Some(limit) = self.record_limit {
            f.write_fmt(format_args!(" record_limit: {limit},"))?;
        }
        if let Some(limit) = self.log_limit {
            f.write_fmt(format_args!(" log_limit: {limit},"))?;
        }
        if self.forget_on_drop {
            f.write_fmt(format_args!(" forget_on_drop: true,"))?;
        }
        f.write_fmt(format_args!("}}",))
    }
}

impl Command {
    /// Creates a new `Command` for launching the `program`. This has no
    /// preprocessing of the input like [Command::new] does.
    ///
    /// The default configuration is to inherit the current process's
    /// environment, and working directory.
    pub fn new_os_str(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().into(),
            ..Default::default()
        }
    }

    /// Creates a `Command` that only sets the `program` and `args` and leaves
    /// other things as their default values. `program_with_args` is separated
    /// by whitespace, the first part becomes the progam, and the the others
    /// are inserted as args.
    ///
    /// In case an argument has spaces, it should be put into `args` as an
    /// unbroken `&str`. In case the command name has spaces, `self.command`
    /// can be changed directly.
    pub fn new(program_with_args: impl AsRef<str>) -> Self {
        let mut program = String::new();
        let mut args: Vec<OsString> = vec![];
        for (i, part) in program_with_args.as_ref().split_whitespace().enumerate() {
            if i == 0 {
                part.clone_into(&mut program)
            } else {
                args.push(part.into());
            }
        }
        Self {
            program: program.into(),
            args,
            ..Default::default()
        }
    }

    /// Adds an argument
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().into());
        self
    }

    /// Adds arguments to be passed to the program
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|s| s.as_ref().into()));
        self
    }

    /// Sets `self.cwd`
    pub fn cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = Some(cwd.as_ref().to_owned());
        self
    }

    /// Set if environment variables should be cleared
    pub fn env_clear(mut self, env_clear: bool) -> Self {
        self.env_clear = env_clear;
        self
    }

    /// Adds an environment variable
    pub fn env(mut self, env_key: impl AsRef<OsStr>, env_val: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((env_key.as_ref().into(), env_val.as_ref().into()));
        self
    }

    /// Adds environment variables
    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.envs.extend(
            envs.into_iter()
                .map(|(k, v)| (k.as_ref().into(), v.as_ref().into())),
        );
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

    /// Sets `forget_on_drop`
    pub fn forget_on_drop(mut self, forget_on_drop: bool) -> Self {
        self.forget_on_drop = forget_on_drop;
        self
    }

    /// Changes the debug line prefix for stdout lines. If `None`, then the
    /// default of the command name and process ID is used.
    pub fn stdout_debug_line_prefix(mut self, line_prefix: Option<String>) -> Self {
        self.stdout_debug_line_prefix = line_prefix;
        self
    }

    /// Changes the debug line prefix for stderr lines. If `None`, then the
    /// default of the command name and process ID is used.
    pub fn stderr_debug_line_prefix(mut self, line_prefix: Option<String>) -> Self {
        self.stderr_debug_line_prefix = line_prefix;
        self
    }

    /// Gets the program and args interspersed with spaces
    pub(crate) fn get_unified_command(&self) -> String {
        let mut command = self.program.to_string_lossy().into_owned();
        if !self.args.is_empty() {
            command += " ";
            for (i, arg) in self.args.iter().enumerate() {
                command += arg.to_string_lossy().as_ref();
                if i != (self.args.len() - 1) {
                    command += " ";
                }
            }
        }
        command
    }

    /// Runs the command with a standard input, returning a `CommandRunner`
    pub async fn run_with_stdin<C: Into<Stdio>>(self, stdin_cfg: C) -> Result<CommandRunner> {
        command_runner(self, stdin_cfg).await
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
            .stack_err_locationless("Command::run_to_completion")?
            .wait_with_output()
            .await
    }

    /// Same as [Command::run_to_completion] except it pipes `input` to the
    /// process stdin
    pub async fn run_with_input_to_completion(self, input: &[u8]) -> Result<CommandResult> {
        let mut runner = self
            .run_with_stdin(Stdio::piped())
            .await
            .stack_err_locationless("Command::run_with_input_to_completion")?;
        let mut stdin = runner.child_process.as_mut().unwrap().stdin.take().unwrap();
        stdin.write_all(input).await.stack_err_locationless(
            "Command::run_with_input_to_completion -> failed to write_all to process stdin",
        )?;
        // needs to close to actually finish
        drop(stdin);
        runner.wait_with_output().await
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
        f.write_fmt(format_args!(
            "CommandResult {{\ncommand: {:?},\nstatus: {:?},\n",
            self.command, self.status
        ))?;
        // move the commas out of the way of the stdout and stderr
        let stdout = self.stdout_as_utf8_lossy();
        if !stdout.is_empty() {
            f.write_fmt(format_args!("stdout: {}\n,", stdout))?;
        }
        let stderr = self.stderr_as_utf8_lossy();
        if !stderr.is_empty() {
            f.write_fmt(format_args!("stderr: {}\n,", stderr))?;
        }
        f.write_fmt(format_args!("}}"))
    }
}

impl Display for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{:#?}", self))
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
                bail_locationless!("{self:#?}.assert_success() -> unsuccessful")
            }
        } else {
            bail_locationless!(
                "{self:#?}.assert_success() -> termination was called before completion"
            )
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
                bail_locationless!("{self:#?}.assert_success() -> unsuccessful")
            }
        } else {
            bail_locationless!(
                "{self:#?}.assert_success() -> termination was called before completion"
            )
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
