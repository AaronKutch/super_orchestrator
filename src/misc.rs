use std::{
    any::type_name,
    fmt,
    fmt::Debug,
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

/// Takes the SHA3-256 hash of the type name of `T` and returns it. Has the
/// potential to change between compiler versions.
pub fn type_hash<T: ?Sized>() -> [u8; 32] {
    use sha3::{Digest, Sha3_256};
    let name = type_name::<T>();
    let mut hasher = Sha3_256::new();
    hasher.update(name.as_bytes());
    hasher.finalize().into()
}

/// For implementing `Debug`, this wrapper makes strings use their `Display`
/// impl rather than `Debug` impl
pub struct DisplayStr<'a>(pub &'a str);
impl<'a> Debug for DisplayStr<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
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
/// `num_retries` is reached in which a timeout error is returned.
///
/// # Example
///
/// This is the definition of `wait_for_ok_lookup_host`
/// ```
/// pub async fn wait_for_ok_lookup_host(
///     num_retries: u64,
///     delay: Duration,
///     host: &str,
/// ) -> Result<SocketAddr> {
///     async fn f(host: &str) -> Result<SocketAddr> {
///         match lookup_host(host).await {
///             Ok(mut addrs) => {
///                 if let Some(addr) = addrs.next() {
///                     Ok(addr)
///                 } else {
///                     Err(Error::from("empty addrs"))
///                 }
///             }
///             Err(e) => Err(e).map_add_err(|| "wait_for_ok_lookup_host(.., host: {host})"),
///         }
///     }
///     wait_for_ok(num_retries, delay, || f(host)).await
/// }
/// ```
#[track_caller]
pub async fn wait_for_ok<F: FnMut() -> Fut, Fut: Future<Output = Result<T>>, T>(
    num_retries: u64,
    delay: Duration,
    mut f: F,
) -> Result<T> {
    let mut i = num_retries;
    loop {
        match f().await {
            Ok(o) => return Ok(o),
            Err(e) => {
                if i == 0 {
                    return Err(e.chain_errors(Error::timeout())).map_add_err(|| {
                        format!(
                            "wait_for_ok(num_retries: {num_retries}, delay: {delay:?}) timeout, \
                             last error stack was"
                        )
                    })
                }
                i -= 1;
            }
        }
        // for `num_retries` we have the check afterwards so that 0 retries can still
        // pass
        sleep(delay).await;
    }
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
