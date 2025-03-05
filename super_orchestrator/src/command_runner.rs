use core::fmt;
use std::{collections::VecDeque, fmt::Debug, process::Stdio, sync::Arc, time::Duration};

use stacked_errors::{bail_locationless, Error, Result, StackableErr};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{self, Child},
    sync::Mutex,
    task::{self, JoinHandle},
    time::{sleep, timeout},
};
use tracing::warn;

use crate::{acquire_dir_path, next_terminal_color, Command, CommandResult};

// note that most things should use `_locationless`, especially if they are
// expected to be able to error under normal `Command` running circumstances,
// the string info should be enough

// TODO IIRC starting lines could be cutoff, this may be because both recorders
// both truncate the file, stepping on each other's first output. Perhaps have
// an `Arc<AtomicBool>` or something to communicate, and change one of the
// `FileOptions` to not truncate?.

/// Used as the engine in the stdout and stderr recording tasks. `unwrap`s only
/// are used in here because it is spawned as a separate task.
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

pub(crate) async fn command_runner<C: Into<Stdio>>(
    this: Command,
    stdin_cfg: C,
) -> Result<CommandRunner> {
    let mut cmd = process::Command::new(&this.program);
    if this.env_clear {
        // must happen before the `envs` call
        cmd.env_clear();
    }
    if let Some(ref cwd) = this.cwd {
        let cwd = acquire_dir_path(cwd)
            .await
            .stack_err_with_locationless(|| {
                format!("{this:?}.run() -> failed to acquire current working directory")
            })?;
        cmd.current_dir(cwd);
    }
    // do as much as possible before spawning the process
    let stdout_log = if let Some(ref options) = this.stdout_log {
        Some(options.acquire_file().await?)
    } else {
        None
    };
    let stderr_log = if let Some(ref options) = this.stderr_log {
        Some(options.acquire_file().await?)
    } else {
        None
    };
    let stdout_record = Arc::new(Mutex::new(VecDeque::new()));
    let stdout_record_clone = if this.stdout_recording && (this.record_limit != Some(0)) {
        Some(Arc::clone(&stdout_record))
    } else {
        None
    };
    let stderr_record = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_record_clone = if this.stderr_recording && (this.record_limit != Some(0)) {
        Some(Arc::clone(&stderr_record))
    } else {
        None
    };
    let record_limit = this.record_limit;
    let log_limit = this.log_limit;
    let program_name = this.program.to_string_lossy();
    let read_loop_timeout = this.read_loop_timeout;
    let mut handles: Vec<JoinHandle<()>> = vec![];
    cmd.args(&this.args)
        .envs(this.envs.iter().map(|x| (&x.0, &x.1)))
        .kill_on_drop(!this.forget_on_drop);
    let mut child = cmd
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .stack_err_with_locationless(|| {
            format!("{this:?}.run() -> failed to spawn child process")
        })?;
    let child_id = child.id().unwrap();
    let terminal_color = if this.stdout_debug || this.stderr_debug {
        next_terminal_color()
    } else {
        owo_colors::AnsiColors::Default
    };
    let stdout_forward = if this.stdout_debug {
        let stdout = tokio::io::stdout();
        // TODO tokio does not support `IsTerminal` yet
        let prefix = if let Some(prefix) = &this.stdout_debug_line_prefix {
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
    let stderr_forward = if this.stderr_debug {
        let stderr = tokio::io::stderr();
        let prefix = if let Some(prefix) = &this.stderr_debug_line_prefix {
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
    if this.stdout_recording || this.stdout_debug || this.stdout_log.is_some() {
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
    if this.stderr_recording || this.stderr_debug || this.stderr_log.is_some() {
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
        command: Some(this),
        child_process: Some(child),
        handles,
        stdout_record,
        stderr_record,
        result: None,
    })
}

/// Note: there are `send_unix_signal` and `send_unix_sigterm` function that can
/// be enabled by the "nix_support" feature
impl CommandRunner {
    /// Attempts to force the command to exit, but does not wait for the request
    /// to take effect. This does not set `self.result`.
    pub fn start_terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.start_kill().stack_err(
                "CommandRunner::start_terminate -> running `start_kill` on the child process \
                 failed",
            )
        } else {
            Ok(())
        }
    }

    /// Forces the command to exit. Drops the internal handle. Returns an error
    /// if some termination method has already been called (this will not
    /// error if the process exited by itself, only if a termination function
    /// that removes the handle has been called).
    ///
    /// `self.result` is set, and `self.result.status` is set to `None`.
    pub async fn terminate(&mut self) -> Result<()> {
        if let Some(child_process) = self.child_process.as_mut() {
            child_process.kill().await.stack_err(
                "CommandRunner::terminate -> running `kill` on the child process failed",
            )?;
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
            bail_locationless!(
                "CommandRunner::terminate -> a termination method has already been called"
            )
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
                        .stack_err("CommandRunner::send_unix_signal -> PID overflow")?,
                )
                .stack_err("CommandRunner::send_unix_signal -> PID creation fail")?,
            ),
            unix_signal,
        )
        .stack_err("CommandRunner::send_unix_signal -> `nix::sys::signal::kill` failed")?;
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
            .stack_err_locationless(
                "`CommandRunner` has already had some termination method called",
            )?
            .wait_with_output()
            .await
            .stack_err_with_locationless(|| {
                format!("{self:?}.wait_with_output() -> failed when waiting on child process")
            })?;
        while let Some(handle) = self.handles.pop() {
            handle.await.stack_err_with_locationless(|| {
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
                .stack_err_locationless(
                    "CommandRunner::wait_with_timeout -> some termination method has already been \
                     called",
                )?
                .try_wait()
            {
                Ok(o) => {
                    if o.is_some() {
                        break
                    }
                }
                Err(e) => {
                    return Err(Error::from_err_locationless(e)).stack_err_locationless(
                        "CommandRunner::wait_with_timeout failed at `try_wait` before reaching \
                         timeout or completed command",
                    )
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

    /// After [CommandRunner::wait_with_timeout] is successful, this will return
    /// a reference to the `CommandResult`
    pub fn get_command_result(&mut self) -> Option<&CommandResult> {
        self.result.as_ref()
    }

    /// After [CommandRunner::wait_with_timeout] is successful, this will take
    /// the `CommandResult` from `self`, replacing it with `None`.
    pub fn take_command_result(&mut self) -> Option<CommandResult> {
        self.result.take()
    }
}
