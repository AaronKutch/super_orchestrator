use std::{
    any::type_name,
    collections::HashSet,
    fmt,
    fmt::Debug,
    future::Future,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(crate) use color_cycle::next_terminal_color;
use stacked_errors::{Error, MapAddError, Result};
use tokio::{
    fs::{read_dir, remove_file, File},
    io::AsyncWriteExt,
    time::sleep,
};

use crate::{acquire_dir_path, Command};

/// use the "ctrlc_support" feature to see functions that use this
pub static CTRLC_ISSUED: AtomicBool = AtomicBool::new(false);

/// Sets up the ctrl-c handler
#[cfg(feature = "ctrlc_support")]
pub fn ctrlc_init() -> Result<()> {
    ctrlc::set_handler(move || {
        CTRLC_ISSUED.store(true, Ordering::SeqCst);
    })?;
    Ok(())
}

/// Sets up `env_logger` with `LevelFilter::Info`
#[cfg(feature = "env_logger_support")]
pub fn std_init() -> Result<()> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();
    Ok(())
}

/// Returns if `CTRLC_ISSUED` has been set, and resets it to `false`
pub fn ctrlc_issued_reset() -> bool {
    CTRLC_ISSUED.swap(false, Ordering::SeqCst)
}

/// Takes the hash of the type name of `T` and returns it. Has the
/// potential to change between compiler versions.
pub fn type_hash<T: ?Sized>() -> [u8; 16] {
    // we can't make this `const` currently because of `type_name`, however it
    // should compile down to the result in practice, at least on release mode
    use sha3::{Digest, Sha3_256};
    let name = type_name::<T>();
    let mut hasher = Sha3_256::new();
    hasher.update(name.as_bytes());
    let tmp: [u8; 32] = hasher.finalize().into();
    let mut res = [0u8; 16];
    res.copy_from_slice(&tmp[0..16]);
    res
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

pub async fn sh_no_dbg(cmd_with_args: &str, args: &[&str]) -> Result<String> {
    let comres = Command::new(cmd_with_args, args)
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
/// use std::{net::SocketAddr, time::Duration};
///
/// use stacked_errors::{Error, MapAddError, Result};
/// use super_orchestrator::wait_for_ok;
/// use tokio::net::lookup_host;
///
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
///             Err(e) => {
///                 Err(e).map_add_err(|| format!("wait_for_ok_lookup_host(.., host: {host})"))
///             }
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

/// This is a guarded kind of removal that only removes all files in a directory
/// with extensions matching the given `extensions`. If a file does not have an
/// extension, it matches against the whole file name.
///
/// e.x. `remove_files_in_dir("./logs", &["log"]).await?;`
pub async fn remove_files_in_dir(dir: &str, extensions: &[&str]) -> Result<()> {
    let dir = acquire_dir_path(dir).await.map_add_err(|| ())?;
    let mut iter = read_dir(dir.clone()).await.map_add_err(|| ())?;
    let mut set = HashSet::new();
    for extension in extensions {
        set.insert(extension.to_string());
    }
    loop {
        let entry = iter.next_entry().await.map_add_err(|| ())?;
        if let Some(entry) = entry {
            let file_type = entry.file_type().await.map_add_err(|| ())?;
            if file_type.is_file() {
                if let Some(name) = entry.file_name().as_os_str().to_str() {
                    let file_only_path = PathBuf::from(name.to_owned());
                    let mut rm_file = false;
                    if let Some(extension) = file_only_path.extension() {
                        if set.contains(extension.to_str().unwrap()) {
                            rm_file = true;
                        }
                    } else if set.contains(file_only_path.to_str().unwrap()) {
                        rm_file = true;
                    }
                    if rm_file {
                        let mut combined = dir.clone();
                        combined.push(file_only_path);
                        remove_file(combined).await.map_add_err(|| ())?;
                    }
                }
            }
        } else {
            break
        }
    }
    Ok(())
}

mod color_cycle {
    use std::sync::atomic::AtomicUsize;

    use owo_colors::{AnsiColors, AnsiColors::*};

    const COLOR_CYCLE: [AnsiColors; 8] = [
        White,
        Yellow,
        Green,
        Cyan,
        BrightBlack,
        Blue,
        BrightCyan,
        BrightGreen,
    ];

    static COLOR_NUM: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn next_terminal_color() -> AnsiColors {
        let inx = COLOR_NUM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        COLOR_CYCLE[inx % COLOR_CYCLE.len()]
    }
}
