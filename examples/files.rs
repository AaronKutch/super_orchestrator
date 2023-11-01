use stacked_errors::{ensure, ensure_eq, StackableErr};
use super_orchestrator::{
    acquire_dir_path, acquire_file_path, acquire_path, stacked_errors::Result,
};

#[rustfmt::skip]
#[tokio::main]
async fn main() -> Result<()> {
    // often we can use it just to check for the existence of a path and return a
    // stacked error if there is an issue.
    acquire_path("./examples/").await.stack()?;
    acquire_path("./examples/files.rs").await.stack()?;

    ensure!(acquire_path("./examples/files.rs")
        .await
        .unwrap()
        .ends_with("super_orchestrator/examples/files.rs"));

    // normalization is performed, note it always returns an absolute path but we
    // are testing only the ends for testing purposes.
    ensure!(acquire_path("./examples/../examples/../examples/files.rs")
        .await
        .unwrap()
        .ends_with("super_orchestrator/examples/files.rs"));

    ensure_eq!(
        format!(
            "{}",
            acquire_path("./examples/nonexistent.rs").await.unwrap_err()
        ),
        r#"Error { stack: [
acquire_path(path_str: "./examples/nonexistent.rs")
Location { file: "/home/admin/Documents/GitHub/super_orchestrator/src/paths.rs", line: 16, col: 10 },
BoxedError(Os { code: 2, kind: NotFound, message: "No such file or directory" }),
] }"#
    );

    // the `_dir_` version insures it is only a directory
    acquire_dir_path("./examples/").await.stack()?;

    ensure!(acquire_dir_path("./examples")
        .await
        .unwrap()
        .ends_with("super_orchestrator/examples"));

    ensure!(acquire_path("./examples/../examples/../examples/")
        .await
        .unwrap()
        .ends_with("super_orchestrator/examples/"));

        ensure_eq!(
        format!("{}", acquire_dir_path("./nonexistent").await.unwrap_err()),
        r#"Error { stack: [
acquire_dir_path(dir_path_str: "./nonexistent")
Location { file: "/home/admin/Documents/GitHub/super_orchestrator/src/paths.rs", line: 45, col: 10 },
BoxedError(Os { code: 2, kind: NotFound, message: "No such file or directory" }),
] }"#
    );

    ensure_eq!(
        format!(
            "{}",
            acquire_dir_path("./examples/files.rs").await.unwrap_err()
        ),
        r#"Error { stack: [
Location { file: "/home/admin/Documents/GitHub/super_orchestrator/src/paths.rs", line: 49, col: 13 },
acquire_dir_path(dir_path_str: "./examples/files.rs") -> is not a directory
] }"#
    );

    // the `_file_` version insures it is only a file
    acquire_file_path("./examples/files.rs").await.stack()?;

    ensure!(acquire_file_path("./examples/files.rs")
        .await
        .unwrap()
        .ends_with("super_orchestrator/examples/files.rs"));

    ensure!(
        acquire_file_path("./examples/../examples/../examples/files.rs")
            .await
            .unwrap()
            .ends_with("super_orchestrator/examples/files.rs")
    );

    ensure_eq!(
        format!("{}", acquire_file_path("./nonexistent").await.unwrap_err()),
        r#"Error { stack: [
acquire_file_path(file_path_str: "./nonexistent")
Location { file: "/home/admin/Documents/GitHub/super_orchestrator/src/paths.rs", line: 27, col: 10 },
BoxedError(Os { code: 2, kind: NotFound, message: "No such file or directory" }),
] }"#
    );

    ensure_eq!(
        format!("{}", acquire_file_path("./examples").await.unwrap_err()),
        r#"Error { stack: [
Location { file: "/home/admin/Documents/GitHub/super_orchestrator/src/paths.rs", line: 31, col: 13 },
acquire_file_path(file_path_str: "./examples") -> is not a file
] }"#
    );

    Ok(())
}
