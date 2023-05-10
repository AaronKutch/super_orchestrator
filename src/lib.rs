//! WIP

// TODO
#![allow(ungated_async_fn_track_caller)]

mod command;
mod error;
mod paths;
mod rw_stream;

pub use command::*;
pub mod docker;
pub use error::*;
pub use paths::*;
pub use rw_stream::*;

// Equivalent to calling `Command::new(cmd,
// &[args...]).ci_mode(true).run_to_completion().await?.assert_success()?;
pub async fn sh(cmd: &str, args: &[&str]) -> Result<()> {
    Command::new(cmd, args)
        .ci_mode(true)
        .run_to_completion()
        .await?
        .assert_success()
}
