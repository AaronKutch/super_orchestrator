use core::fmt;
use std::{
    borrow::{Borrow, Cow},
    collections::VecDeque,
    ffi::{OsStr, OsString},
    fmt::{Debug, Display},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    str::Utf8Error,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use stacked_errors::{DisplayStr, Error, Result, StackableErr};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{self, Child},
    sync::Mutex,
    task::{self, JoinHandle},
    time::{sleep, timeout},
};
use tracing::warn;

use crate::{acquire_dir_path, next_terminal_color, FileOptions};

// note that most things should use `_locationless`, especially if they are
// expected to be able to error under normal `Command` running circumstances,
// the string info should be enough

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

/// Detached `Commands` are represented by this struct.
///
/// # Note
///
/// Locks on `stdout_record` and `stderr_record` should only be held long enough
/// to make the needed `VecDeque` operations, because the task to record program
/// outputs needs the lock to progress.
///
/// If the `tracing` crate is used and a subscriber is active, warnings from
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
    /// The handle to the `Child` process. The `ChildStdout` was taken if there
    /// was any kind of recording. `stdout_recording`, `stdout_debug`, and
    /// `stdout_log.is_some()` should all be false in the `Command` if you
    /// want direct access to the `ChildStdout`. Likewise, `stderr_recording`,
    /// `stderr_debug`, and `stderr_log.is_some()` should be all false if
    /// you want `ChildStderr`.
    pub child_process: Option<Child>,
    handles: Vec<tokio::task::JoinHandle<()>>,

    // TODO I'm not sure if this can/should be a `std::sync::mutex` considering the parallel async
    // tasks, clippy sends warnings in basic_commands.rs (not sure if they are spurious).
    /// The stdout of the command is actively pushed to the `VecDeque`. The
    /// remaining contents of this are used in `stdout` in the `CommandResult`.
    /// Note: the lock should only be held long enough to make needed
    /// `VecDeque` operations.
    pub stdout_record: Arc<Mutex<VecDeque<u8>>>,
    /// The stderr of the command is actively pushed to the `VecDeque`. The
    /// remaining contents of this are used in `stderr` in the `CommandResult`.
    /// Note: the lock should only be held long enough to make needed
    /// `VecDeque` operations.
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
    // write point and prefix
    mut std_forward: Option<(W, String)>,
) {
    const FORWARDING_FAILED: &str =
        "`super_orchestrator::Command` stdout or stderr recording failed on write";
    // for tracking how much has been written to the file
    let mut log_len = 0u64;
    // if the previous read had a newline on the end (for forwarding to stdout)
    let mut previous_newline = false;
    // if no bytes have been written (for forwarding to stdout)
    let mut empty = true;
    let mut line_buf = Vec::new();
    // when a utf8 codepoint is cut up across reads, we need to store it here
    let mut cut_up: Option<Vec<u8>> = None;
    // 8 KB, like BufReader
    let mut buf = [0u8; 8 * 1024];
    loop {
        match timeout(read_loop_timeout, std_read.read(&mut buf)).await {
            Ok(Ok(bytes_read)) => {
                if bytes_read == 0 {
                    // if there has been nonempty output with no ending newline insert one upon
                    // completion
                    if (!empty) && (!previous_newline) {
                        if let Some((ref mut std_forward, _)) = std_forward {
                            if cut_up.is_some() {
                                // the outside precondition is always met in case of an incomplete
                                std_forward
                                    .write_all("\u{fffd}\n".as_bytes())
                                    .await
                                    .expect(FORWARDING_FAILED);
                            } else {
                                std_forward.write_all(b"\n").await.expect(FORWARDING_FAILED);
                            }
                            std_forward.flush().await.unwrap();
                        }
                    }
                    break
                }
                let mut bytes = &buf[..bytes_read];
                // copying to record
                if let Some(ref mut arc) = std_record {
                    let mut deque = arc.lock().await;
                    if let Some(limit) = record_limit {
                        let limit = usize::try_from(limit).unwrap();
                        if deque.len().saturating_add(bytes.len()) > limit {
                            // we would overflow the limit if all the `bytes` were inserted
                            if bytes.len() >= limit {
                                // the deque needs to be entirely replaced with the end of `bytes`
                                deque.clear();
                                deque.extend(bytes[bytes.len().wrapping_sub(limit)..].iter());
                            } else {
                                let start =
                                    deque.len().wrapping_sub(limit).wrapping_add(bytes.len());
                                deque.drain(..start);
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
                    let mut reset = false;
                    let len = u64::try_from(bytes.len()).unwrap();
                    log_len = log_len.checked_add(len).unwrap();
                    if let Some(limit) = log_limit {
                        if log_len > limit {
                            reset = true;
                            std_log.set_len(0).await.unwrap();
                            std_log.seek(std::io::SeekFrom::Start(0)).await.unwrap();
                            let start = if len > limit {
                                len.wrapping_sub(limit)
                            } else {
                                0
                            };
                            std_log
                                .write_all(&bytes[usize::try_from(start).unwrap()..])
                                .await
                                .expect(FORWARDING_FAILED);
                            log_len = len.wrapping_sub(start);
                        }
                    }
                    if !reset {
                        std_log.write_all(bytes).await.expect(FORWARDING_FAILED);
                    }
                }
                // copying to std stream
                if let Some((ref mut std_forward, ref prefix)) = std_forward {
                    let mut tmp = Vec::new();
                    if let Some(cut_up) = cut_up.take() {
                        // prepend the possibly cut up bytes, this should be very rare
                        tmp.extend_from_slice(&cut_up);
                        tmp.extend_from_slice(bytes);
                        // use this instead of the original backing to `bytes`
                        bytes = &tmp;
                    }
                    // `utf8_chunks` is incredibly useful, since the `incomplete` function will only
                    // check on the last chunk
                    for utf8_chunk in bstr::ByteSlice::utf8_chunks(bytes) {
                        // `utf8_chunk` can have a valid part followed by an invalid part
                        let valid = utf8_chunk.valid();
                        if !valid.is_empty() {
                            // `lines_with_terminator` avoids the issue with `lines` where a string
                            // with the final sequence being a newline has no difference without it
                            for line in bstr::ByteSlice::lines_with_terminator(valid.as_bytes()) {
                                // Need to write the terminal prefix together with the line,
                                // otherwise stdout running into stderr
                                // is too common. `write_vectored` is useless for this.

                                // if there has been no writing yet, or the last writing had a
                                // newline, then insert the terminal prefix
                                if empty || previous_newline {
                                    line_buf.extend_from_slice(prefix.as_bytes());
                                }
                                previous_newline = line.last() == Some(&b'\n');
                                line_buf.extend_from_slice(line);
                                std_forward
                                    .write_all(&line_buf)
                                    .await
                                    .expect(FORWARDING_FAILED);
                                line_buf.clear();
                                empty = false;
                            }
                        }
                        let invalid = utf8_chunk.invalid();
                        if !invalid.is_empty() {
                            // need to have this again
                            if empty || previous_newline {
                                line_buf.extend_from_slice(prefix.as_bytes());
                            }
                            if utf8_chunk.incomplete() {
                                // the next read pass or ending will pick this up
                                cut_up = Some(invalid.to_vec());
                            } else {
                                // insert replacement character, this will happen according to the
                                // "substitution of maximal subparts" strategy described in `bstr`
                                line_buf.extend_from_slice("\u{fffd}".as_bytes());
                            }
                            if !line_buf.is_empty() {
                                std_forward
                                    .write_all(&line_buf)
                                    .await
                                    .expect(FORWARDING_FAILED);
                                line_buf.clear();
                            }
                            previous_newline = false;
                            empty = false;
                        }
                    }
                    // if set excessively large by some single line, shrink
                    if line_buf.capacity() > (8 * 1024) {
                        line_buf.shrink_to_fit();
                    }
                    std_forward.flush().await.unwrap();
                }
            }
            Ok(Err(e)) => {
                panic!(
                    "`super_orchestrator::Command` stdout or stderr recording failed on read: {}",
                    e
                )
            }
            // timeout
            Err(_) => (),
        }
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
        let mut cmd = process::Command::new(&self.program);
        if self.env_clear {
            // must happen before the `envs` call
            cmd.env_clear();
        }
        if let Some(ref cwd) = self.cwd {
            let cwd = acquire_dir_path(cwd).await.stack_err_locationless(|| {
                format!("{self:?}.run() -> failed to acquire current working directory")
            })?;
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
        let record_limit = self.record_limit;
        let log_limit = self.log_limit;
        let program_name = self.program.to_string_lossy();
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
            .stack_err_locationless(|| {
                format!("{self:?}.run() -> failed to spawn child process")
            })?;
        let child_id = child.id().unwrap();
        let terminal_color = if self.stdout_debug || self.stderr_debug {
            next_terminal_color()
        } else {
            owo_colors::AnsiColors::Default
        };
        let stdout_forward = if self.stdout_debug {
            let stdout = tokio::io::stdout();
            // TODO tokio does not support `IsTerminal` yet
            let prefix = if let Some(prefix) = &self.stdout_debug_line_prefix {
                prefix.clone()
            } else {
                owo_colors::OwoColorize::color(
                    &format!("{program_name} {child_id}  | "),
                    terminal_color,
                )
                .to_string()
            };
            Some((stdout, prefix))
        } else {
            None
        };
        let stderr_forward = if self.stderr_debug {
            let stderr = tokio::io::stderr();
            let prefix = if let Some(prefix) = &self.stderr_debug_line_prefix {
                prefix.clone()
            } else {
                owo_colors::OwoColorize::color(
                    &format!("{program_name} {child_id} E| "),
                    terminal_color,
                )
                .to_string()
            };
            Some((stderr, prefix))
        } else {
            None
        };
        // dropping the stdout and stderr handles actually results in an error, we keep
        // all the stuff anyway in `child_process` if there is not any kind of recording
        if self.stdout_recording || self.stdout_debug || self.stdout_log.is_some() {
            let stdout = child.stdout.take().unwrap();
            let stdout_read = BufReader::new(stdout);
            handles.push(task::spawn(recorder(
                read_loop_timeout,
                stdout_read,
                stdout_record_clone,
                record_limit,
                stdout_log,
                log_limit,
                stdout_forward,
            )));
        }
        if self.stderr_recording || self.stderr_debug || self.stderr_log.is_some() {
            let stderr = child.stderr.take().unwrap();
            let stderr_read = BufReader::new(stderr);
            handles.push(task::spawn(recorder(
                read_loop_timeout,
                stderr_read,
                stderr_record_clone,
                record_limit,
                stderr_log,
                log_limit,
                stderr_forward,
            )));
        }
        Ok(CommandRunner {
            command: Some(self),
            child_process: Some(child),
            handles,
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
            .stack_err_locationless(|| "Command::run_to_completion")?
            .wait_with_output()
            .await
    }

    /// Same as [Command::run_to_completion] except it pipes `input` to the
    /// process stdin
    pub async fn run_with_input_to_completion(self, input: &[u8]) -> Result<CommandResult> {
        let mut runner = self
            .run_with_stdin(Stdio::piped())
            .await
            .stack_err_locationless(|| "Command::run_with_input_to_completion")?;
        let mut stdin = runner.child_process.as_mut().unwrap().stdin.take().unwrap();
        stdin.write_all(input).await.stack_err_locationless(|| {
            "Command::run_with_input_to_completion -> failed to write_all to process stdin"
        })?;
        // needs to close to actually finish
        drop(stdin);
        runner.wait_with_output().await
    }
}

/// Note: there are `send_unix_signal` and `send_unix_sigterm` function that can
/// be enabled by the "nix_support" feature
impl CommandRunner {
    /// Attempts to force the command to exit, but does not wait for the request
    /// to take effect. This does not set `self.result`.
    pub fn start_terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.start_kill().stack_err(|| {
                "CommandRunner::start_terminate -> running `start_kill` on the child process failed"
            })
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
            child_process.kill().await.stack_err(|| {
                "CommandRunner::terminate -> running `kill` on the child process failed"
            })?;
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
            Err(Error::from_kind_locationless(
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
            nix::unistd::Pid::from_raw(
                i32::try_from(
                    self.pid()
                        .stack_err(|| "CommandRunner::send_unix_signal -> PID overflow")?,
                )
                .stack_err(|| "CommandRunner::send_unix_signal -> PID creation fail")?,
            ),
            unix_signal,
        )
        .stack_err(|| "CommandRunner::send_unix_signal -> `nix::sys::signal::kill` failed")?;
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
            .stack_err_locationless(|| {
                "`CommandRunner` has already had some termination method called"
            })?
            .wait_with_output()
            .await
            .stack_err_locationless(|| {
                format!("{self:?}.wait_with_output() -> failed when waiting on child process")
            })?;
        while let Some(handle) = self.handles.pop() {
            handle.await.stack_err_locationless(|| {
                format!("{self:?}.wait_with_output() -> `Command` task panicked")
            })?;
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
    /// status, use `assert_success` or check the `status` on
    /// the `CommandResult`.
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
            match self
                .child_process
                .as_mut()
                .stack_err_locationless(|| {
                    "`CommandRunner` has already had some termination method called"
                })?
                .try_wait()
            {
                Ok(o) => {
                    if o.is_some() {
                        break
                    }
                }
                Err(e) => {
                    return Err(Error::from_kind_locationless(e)).stack_err_locationless(|| {
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
                Err(Error::from_kind_locationless(format!(
                    "{self:#?}.assert_success() -> unsuccessful"
                )))
            }
        } else {
            Err(Error::from_kind_locationless(format!(
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
                Err(Error::from_kind_locationless(format!(
                    "{self:#?}.assert_success() -> unsuccessful"
                )))
            }
        } else {
            Err(Error::from_kind_locationless(format!(
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
