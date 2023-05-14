use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use tokio::{fs::File, io::AsyncWriteExt, time::sleep};

use crate::{Command, Error, MapAddError, Result};

/// use the "ctrlc_support" feature to see functions that use this
pub static CTRLC_ISSUED: AtomicBool = AtomicBool::new(false);

/// Sets up `env_logger` with `LevelFilter::Info` and the ctrl-c handler
#[cfg(all(feature = "ctrlc_support", feature = "env_logger_support"))]
pub fn std_init() -> Result<()> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
    ctrlc::set_handler(move || {
        CTRLC_ISSUED.store(true, Ordering::SeqCst);
    })?;
    Ok(())
}

/// Returns if `CTRLC_ISSUED` has been set, and resets it to `false`
pub fn ctrlc_issued_reset() -> bool {
    CTRLC_ISSUED.swap(false, Ordering::SeqCst)
}

/// Equivalent to calling `Command::new(cmd_with_args,
/// &[args...]).ci_mode(true).run_to_completion().await?.assert_success()?;` and
/// returning the stdout
pub async fn sh(cmd_with_args: &str, args: &[&str]) -> Result<String> {
    let comres = Command::new(cmd_with_args, args)
        .ci_mode(true)
        .run_to_completion()
        .await?;
    comres.assert_success()?;
    Ok(comres.stdout)
}

pub const STD_TRIES: u64 = 300;
pub const STD_DELAY: Duration = Duration::from_millis(300);

/// Repeatedly polls `f` until it returns an `Ok` which is returned, or
/// `num_tries` is reached in which a timeout error is returned
#[track_caller]
pub async fn wait_for_ok<F: FnMut() -> Fut, Fut: Future<Output = Result<T>>, T>(
    num_tries: u64,
    delay: Duration,
    mut f: F,
) -> Result<T> {
    for _ in 0..num_tries {
        if let Ok(o) = f().await {
            return Ok(o)
        }
        sleep(delay).await;
    }
    Err(Error::timeout().add_err(format!(
        "wait_for_ok(num_tries: {num_tries}, delay: {delay:?}) timeout"
    )))
}

/// First, this splits by `separate`, trims outer whitespace, sees if `key` is
/// prefixed, if so it also strips `inter_key_val` and returns the stripped and
/// trimmed value.
///```
/// use super_orchestrator::get_separated_val;
///
/// let s = "\
///     address:    0x2b4e4d79e3e9dBBB170CCD78419520d1DCBb4B3f\npublic  : 0x04b141241511b1\n  \
///          private  :=\"hello world\" \n";
/// assert_eq!(
///     &get_separated_val(s, "\n", "address", ":").unwrap(),
///     "0x2b4e4d79e3e9dBBB170CCD78419520d1DCBb4B3f"
/// );
/// assert_eq!(
///     &get_separated_val(s, "\n", "public", ":").unwrap(),
///     "0x04b141241511b1"
/// );
/// assert_eq!(
///     &get_separated_val(s, "\n", "private", ":=").unwrap(),
///     "\"hello world\""
/// );
/// ```
#[track_caller]
pub fn get_separated_val(
    input: &str,
    separate: &str,
    key: &str,
    inter_key_val: &str,
) -> Result<String> {
    let mut value = None;
    for line in input.split(separate) {
        if let Some(x) = line.trim().strip_prefix(key) {
            if let Some(y) = x.trim().strip_prefix(inter_key_val) {
                value = Some(y.trim().to_owned());
                break
            }
        }
    }
    value.map_add_err(|| format!("get_separated_val() -> key \"{key}\" not found"))
}

/// Closing files is a tricky thing (I think (?) the `sync_all` part can even
/// apply to read-only files because of the openness static) if syncronization
/// with other programs is required, this function makes sure changes are
/// flushed and `sync_all` is called to make sure the data has actually been
/// written to filesystem.
pub async fn close_file(mut file: File) -> Result<()> {
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}
