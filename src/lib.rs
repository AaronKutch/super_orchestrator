//! WIP

// TODO
#![allow(ungated_async_fn_track_caller)]

mod command;
mod error;
mod log_file;
mod paths;

pub use command::*;
pub mod docker;
pub use error::*;
pub use log_file::*;
pub use paths::*;

// Equivalent to calling `Command::new(cmd,
// &[args...]).ci_mode(true).run_to_completion().await?.assert_success()?;
pub async fn sh(cmd: &str, args: &[&str]) -> Result<()> {
    Command::new(cmd, args)
        .ci_mode(true)
        .run_to_completion()
        .await?
        .assert_success()
}
