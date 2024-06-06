use std::path::PathBuf;

use stacked_errors::{ensure, ensure_eq, StackableErr};
use super_orchestrator::{
    close_file, remove_files_in_dir, stacked_errors::Result, FileOptions, ReadOrWrite,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
};

#[tokio::main]
#[rustfmt::skip]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    remove_files_in_dir("./logs/", &["example.log"])
        .await
        .stack()?;

    // In the "Rust scripts" super_orchestrator was designed for, often we want to
    // read from or write to a file. Most of the time, we want the `create` and
    // `!append` options, to write_all (and not accidentally use one of the
    // `AsyncWriteExt` functions that may not write all of the buffer in one call),
    // close the file correctly so that there cannot be wild filesystem race
    // conditions, and handle all the fallible points somehow.

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("./logs/example.log")
        .await
        .stack()?;
    file.write_all("test".as_bytes()).await.stack()?;
    file.flush().await.stack()?;
    file.sync_all().await.stack()?;
    drop(file);

    let mut file = OpenOptions::new()
        .read(true)
        .open("./logs/example.log")
        .await
        .stack()?;
    let mut s = String::new();
    file.read_to_string(&mut s).await.stack()?;
    drop(file);
    ensure_eq!(s, "test");

    remove_files_in_dir("./logs/", &["example.log"])
        .await
        .stack()?;

    // instead, `FileOptions` allows you to do equivalently do this
    FileOptions::write_str("./logs/example.log", "test")
        .await
        .stack()?;
    let s = FileOptions::read_to_string("./logs/example.log")
        .await
        .stack()?;
    ensure_eq!(s, "test");

    // it gives structured errors
    let e = FileOptions::write_str("./nonexistent/example.log", "test")
        .await
        .stack()
        .unwrap_err()
        .to_string();
    println!("{}", e);
    // (omitting the line number and OS error from the test, but see the printed
    // result)
    ensure!(
        e.contains(r#"FileOptions::write_str
FileOptions::acquire_file()
FileOptions { path: "./nonexistent/example.log", options: Write(WriteOptions { create: true, append: false }) }.preacquire() could not acquire directory
acquire_dir_path(dir_path: "./nonexistent")
BoxedError"#)
    );

    let e = FileOptions::read_to_string("./logs/nonexistent.log")
        .await
        .stack()
        .unwrap_err()
        .to_string();
    println!("{}", e);
    // (omitting the line number and OS error from the test, but see the printed
    // result)
    ensure!(
        e.contains(r#"FileOptions::read_to_string
FileOptions::acquire_file()
FileOptions { path: "./logs/nonexistent.log", options: Read }.precheck() could not acquire path to combined directory and file name
acquire_file_path(file_path:"#)
    );

    // the shorthand functions can be broken down into more steps if needed

    let file_path: PathBuf = FileOptions::read("./logs/example.log")
        .preacquire()
        .await
        .stack()?;
    println!("checked path: {file_path:?}");

    let file: File = FileOptions::read("./logs/example.log")
        .acquire_file()
        .await
        .stack()?;
    println!("file: {file:?}");

    let mut file = FileOptions::new("./logs/example.log", ReadOrWrite::write(false, true))
        .acquire_file()
        .await
        .stack()?;
    file.write_all(" part 2".as_bytes()).await.stack()?;
    close_file(file).await.stack()?;

    ensure_eq!(
        FileOptions::read_to_string("./logs/example.log")
            .await
            .stack()?,
        "test part 2"
    );

    Ok(())
}
