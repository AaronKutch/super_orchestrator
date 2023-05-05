use core::fmt;
use std::{
    path::Path,
    process::{ExitStatus, Stdio},
};

use tokio::{
    fs::File,
    io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, Command},
    task,
};

pub fn assert_dir_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        Err(format!("path {} does not exist", path.to_str().unwrap()))
    } else if !path.is_dir() {
        Err(format!(
            "path {} is not a directory",
            path.to_str().unwrap()
        ))
    } else {
        Ok(())
    }
}

pub fn assert_file_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        Err(format!("path {} does not exist", path.to_str().unwrap()))
    } else if !path.is_file() {
        Err(format!("path {} is not a file", path.to_str().unwrap()))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct ComplexCommand {
    pub command: String,
    pub args: Vec<String>,
    pub child: Option<Child>,
    pub handles: Vec<tokio::task::JoinHandle<()>>,
    pub ci: bool,
}

/// A more convenient command result that is returned in both failure and
/// successful cases
#[derive(Debug)]
pub struct ComplexOutput {
    /// This is nonempty when there was some failure of the command itself or
    /// something else in the `ComplexCommand`
    pub complex_err: String,
    pub status: Option<ExitStatus>,
    pub stdout: String,
    pub stderr: String,
}

impl fmt::Display for ComplexOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl ComplexCommand {
    /// Spawns a command with piped stdout and stderr. If `wait` or
    /// `wait_for_output` is not called this is left detached. Per
    /// `ComplexCommand::new`, only one from each of the the `stdout_`,
    /// `stderr_`, and `wait_` classes of functions should be called. `ci` makes
    /// `stdout_` and `stderr_` functions pipe to the current process.
    pub fn new(command: &str, args: &[&str], ci: bool) -> Result<Self, String> {
        let child = Command::new(command)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn command {} {:?}: {}", command, args, e))?;
        Ok(Self {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            child: Some(child),
            handles: vec![],
            ci,
        })
    }

    /// Spawns a task to continuously copy the stdout to a file
    pub async fn stdout_to_file(mut self, path: &Path) -> Result<Self, String> {
        let mut stdout = self.child.as_mut().unwrap().stdout.take().unwrap();
        if self.ci {
            // in CI mode print to stdout
            let prefix = path.file_name().unwrap().to_str().unwrap().to_owned();
            let mut lines = BufReader::new(stdout).lines();
            let mut writer = BufWriter::new(tokio::io::stdout());
            self.handles.push(task::spawn(async move {
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let _ = writer
                                .write(format!("{} | {}\n", prefix, line).as_bytes())
                                .await
                                .unwrap();
                            writer.flush().await.unwrap();
                        }
                        Ok(None) => break,
                        Err(e) => panic!("stdout line copier failed with {}", e),
                    }
                }
            }));
        } else {
            let mut file = File::create(path)
                .await
                .map_err(|e| format!("failed to create stdout file: {}", e))?;
            self.handles.push(task::spawn(async move {
                io::copy(&mut stdout, &mut file).await.unwrap();
            }));
        }
        Ok(self)
    }

    /// Spawns a task to continuously copy the stderr to a file
    pub async fn stderr_to_file(mut self, path: &Path) -> Result<Self, String> {
        let mut stderr = self.child.as_mut().unwrap().stderr.take().unwrap();
        if self.ci {
            let prefix = path.file_name().unwrap().to_str().unwrap().to_owned();
            let mut lines = BufReader::new(stderr).lines();
            let mut writer = BufWriter::new(tokio::io::stderr());
            self.handles.push(task::spawn(async move {
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let _ = writer
                                .write(format!("{} | {}\n", prefix, line).as_bytes())
                                .await
                                .unwrap();
                            writer.flush().await.unwrap();
                        }
                        Ok(None) => break,
                        Err(e) => panic!("stderr line copier failed with {}", e),
                    }
                }
            }));
        } else {
            let mut file = File::create(path)
                .await
                .map_err(|e| format!("failed to create stderr file: {}", e))?;
            self.handles.push(task::spawn(async move {
                io::copy(&mut stderr, &mut file).await.unwrap();
            }));
        }
        Ok(self)
    }

    /// On success the stdout is returned. The stderr is returned as the second
    /// tuple element in both cases
    pub async fn wait_for_output(mut self) -> Result<ComplexOutput, ComplexOutput> {
        let output = match self.child.take().unwrap().wait_with_output().await {
            Ok(o) => o,
            Err(e) => {
                return Err(ComplexOutput {
                    complex_err: format!("failed when waiting on child: {}", e),
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        };
        let mut complex_output = ComplexOutput {
            complex_err: String::new(),
            status: Some(output.status),
            stdout: String::new(),
            stderr: String::new(),
        };
        if let Ok(stderr) = String::from_utf8(output.stderr.clone()) {
            complex_output.stderr = stderr;
        } else {
            complex_output.complex_err = format!("failed to parse stderr as utf8: {:?}", output);
            return Err(complex_output)
        }
        if let Ok(stdout) = String::from_utf8(output.stdout.clone()) {
            complex_output.stdout = stdout;
        } else {
            complex_output.complex_err = format!("failed to parse stdout as utf8: {:?}", output);
            return Err(complex_output)
        }
        if !output.status.success() {
            complex_output.complex_err = format!(
                "`{} {:?}` command returned exit status {}",
                self.command, self.args, output.status
            );
            return Err(complex_output)
        }
        while let Some(handle) = self.handles.pop() {
            match handle.await {
                Ok(()) => (),
                Err(e) => {
                    complex_output.complex_err = format!("`ComplexCommand` task panicked: {}", e);
                    return Err(complex_output)
                }
            }
        }
        Ok(complex_output)
    }

    /// Waits for successful completion, or returns an error
    pub async fn wait(mut self) -> Result<(), String> {
        let exit_status = self
            .child
            .take()
            .unwrap()
            .wait()
            .await
            .map_err(|e| format!("failed when waiting on child: {}", e))?;
        let res = if exit_status.success() {
            Ok(())
        } else {
            Err(format!(
                "`{} {:?}` command returned exit status {}",
                self.command, self.args, exit_status
            ))
        };
        while let Some(handle) = self.handles.pop() {
            handle
                .await
                .map_err(|e| format!("`ComplexCommand` task panicked: {}", e))?;
        }
        res
    }
}
