use std::{
    any::type_name,
    collections::HashSet,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(crate) use color_cycle::next_terminal_color;
use stacked_errors::{Error, ErrorKind, Result, StackableErr};
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
    })
    .stack()?;
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

/// Equivalent to calling `Command::new(cmd_with_args,
/// &[args...]).debug(true).run_to_completion().await?.assert_success()?;` and
/// returning the stdout as a `String` (returns an error if the stdout was not
/// utf-8)
pub async fn sh(cmd_with_args: &str, args: &[&str]) -> Result<String> {
    let comres = Command::new(cmd_with_args, args)
        .debug(true)
        .run_to_completion()
        .await?;
    comres.assert_success()?;
    comres
        .stdout_as_utf8()
        .map(|s| s.to_owned())
        .stack_err_locationless(|| "`Command` output was not UTF-8")
}

/// [sh] but without debug mode
pub async fn sh_no_debug(cmd_with_args: &str, args: &[&str]) -> Result<String> {
    let comres = Command::new(cmd_with_args, args)
        .run_to_completion()
        .await?;
    comres.assert_success()?;
    comres
        .stdout_as_utf8()
        .map(|s| s.to_owned())
        .stack_err_locationless(|| "`Command` output was not UTF-8")
}

/// Repeatedly polls `f` until it returns an `Ok` which is returned, or
/// `num_retries` is reached in which a timeout error is returned.
///
/// # Example
///
/// This is the definition of `wait_for_ok_lookup_host`
/// ```
/// use std::{net::SocketAddr, time::Duration};
///
/// use stacked_errors::{Error, Result, StackableErr};
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
///             Err(e) => Err(Error::from(e))
///                 .stack_err(|| format!("wait_for_ok_lookup_host(.., host: {host})")),
///         }
///     }
///     wait_for_ok(num_retries, delay, || f(host)).await
/// }
/// ```
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
                    return Err(e.add_kind_locationless(ErrorKind::TimeoutError)).stack_err(|| {
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

/// This function makes sure changes are flushed and `sync_all` is called to
/// make sure the file has actually been completely written to the filesystem
/// and closed before the end of this function.
pub async fn close_file(mut file: File) -> Result<()> {
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

/// This is a guarded kind of removal that only removes all files in a directory
/// that match an element of `ends_with`. If the element starts with ".",
/// extensions are matched against, otherwise whole file names are matched
/// against. Only whole extension components are matched against.
///
/// # Example
///
/// ```no_run
/// use super_orchestrator::{
///     acquire_file_path, remove_files_in_dir,
///     stacked_errors::{ensure, Result},
///     FileOptions,
/// };
/// async fn ex() -> Result<()> {
///     // note: in regular use you would use `.await.stack()?` on the ends
///     // to tell what lines are failing
///
///     // create some empty example files
///     FileOptions::write_str("./logs/binary", "").await?;
///     FileOptions::write_str("./logs/ex0.log", "").await?;
///     FileOptions::write_str("./logs/ex1.log", "").await?;
///     FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
///     FileOptions::write_str("./logs/tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &["r.gz", ".r.gz"]).await?;
///     // check that files "ex2.tar.gz" and "tar.gz" were not removed
///     // even though "r.gz" is in their string suffixes, because it
///     // only matches against complete extension components.
///     acquire_file_path("./logs/ex2.tar.gz").await?;
///     acquire_file_path("./logs/tar.gz").await?;
///
///     remove_files_in_dir("./logs", &["binary", ".log"]).await?;
///     // check that only the "binary" and all ".log" files were removed
///     ensure!(acquire_file_path("./logs/binary").await.is_err());
///     ensure!(acquire_file_path("./logs/ex0.log").await.is_err());
///     ensure!(acquire_file_path("./logs/ex1.log").await.is_err());
///     acquire_file_path("./logs/ex2.tar.gz").await?;
///     acquire_file_path("./logs/tar.gz").await?;
///
///     remove_files_in_dir("./logs", &[".gz"]).await?;
///     // any thing ending with ".gz" should be gone
///     ensure!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
///     ensure!(acquire_file_path("./logs/tar.gz").await.is_err());
///
///     // recreate some files
///     FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
///     FileOptions::write_str("./logs/ex3.tar.gz.other", "").await?;
///     FileOptions::write_str("./logs/tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &["tar.gz"]).await?;
///     // only the file is matched because the element did not begin with a "."
///     acquire_file_path("./logs/ex2.tar.gz").await?;
///     acquire_file_path("./logs/ex3.tar.gz.other").await?;
///     ensure!(acquire_file_path("./logs/tar.gz").await.is_err());
///
///     FileOptions::write_str("./logs/tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &[".tar.gz"]).await?;
///     // only a strict extension suffix is matched
///     ensure!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
///     acquire_file_path("./logs/ex3.tar.gz.other").await?;
///     acquire_file_path("./logs/tar.gz").await?;
///
///     FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &[".gz", ".other"]).await?;
///     ensure!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
///     ensure!(acquire_file_path("./logs/ex3.tar.gz.other").await.is_err());
///     ensure!(acquire_file_path("./logs/tar.gz").await.is_err());
///
///     Ok(())
/// }
/// ```
///
/// # Errors
///
/// - If any `ends_with` element has more than one component (e.x. if there are
///   any '/' or '\\')
///
/// - If `acquire_dir_path(dir)` fails
pub async fn remove_files_in_dir(dir: impl AsRef<Path>, ends_with: &[&str]) -> Result<()> {
    let mut file_name_set: HashSet<OsString> = HashSet::new();
    let mut extension_set: HashSet<OsString> = HashSet::new();
    for (i, s) in ends_with.iter().enumerate() {
        let mut s = *s;
        if s.is_empty() {
            return Err(Error::from(format!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?}) -> `ends_with` element {} is \
                 empty",
                dir.as_ref(),
                ends_with,
                i
            )))
        }
        let is_extension = s.starts_with('.');
        if is_extension {
            s = &s[1..];
        }
        let path = PathBuf::from(s);
        let mut iter = path.components();
        let component = iter.next().stack_err(|| {
            format!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?}) -> `ends_with` element {} has no \
                 component",
                dir.as_ref(),
                ends_with,
                i
            )
        })?;
        if iter.next().is_some() {
            return Err(Error::from(format!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?}) -> `ends_with` element {} has \
                 more than one component",
                dir.as_ref(),
                ends_with,
                i
            )))
        }
        if is_extension {
            extension_set.insert(component.as_os_str().to_owned());
        } else {
            file_name_set.insert(component.as_os_str().to_owned());
        }
    }

    let dir_path_buf = acquire_dir_path(dir.as_ref()).await.stack_err(|| {
        format!(
            "remove_files_in_dir(dir: {:?}, ends_with: {:?})",
            dir.as_ref(),
            ends_with
        )
    })?;
    let mut iter = read_dir(dir_path_buf.clone()).await.stack()?;
    loop {
        let entry = iter.next_entry().await.stack()?;
        if let Some(entry) = entry {
            let file_type = entry.file_type().await.stack()?;
            if file_type.is_file() {
                let file_only_path = PathBuf::from(entry.file_name());
                // check against the whole file name
                let mut rm_file = file_name_set.contains(file_only_path.as_os_str());
                if !rm_file {
                    // now check against suffixes
                    // the way we do this is check with every possible extension suffix
                    let mut subtracting = file_only_path.clone();
                    let mut suffix = OsString::new();
                    while let Some(extension) = subtracting.extension() {
                        let mut tmp = extension.to_owned();
                        tmp.push(&suffix);
                        suffix = tmp;

                        if extension_set.contains(&suffix) {
                            rm_file = true;
                            break
                        }

                        // remove very last extension as we add on extensions fo `suffix
                        subtracting = PathBuf::from(subtracting.file_stem().unwrap().to_owned());

                        // prepare "." prefix
                        let mut tmp = OsString::from_str(".").unwrap();
                        tmp.push(&suffix);
                        suffix = tmp;
                    }
                }
                if rm_file {
                    let mut combined = dir_path_buf.clone();
                    combined.push(file_only_path);
                    remove_file(combined).await.stack()?;
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
