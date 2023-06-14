# Changelog

## [0.3.0] - TODO
### Changes
- Removed `Command::inherit_stdin` and instead introduced a `run_with_stdin` function that takes
  any `Stdio`. Simply use `.run_with_stdin(Stdio::inherit())` if you want the property
  `inherit_stdin` had.
- Changed `remove_files_in_dir` to also handle files without extensions
- Changed `CommandRunner` termination semantics to error on an already terminated command
  (specifically, not if the underlying process has exited, only if the handle has been dropped
  by a termination function)

### Additions
- `Command::run_with_input_to_completion`
- `CommandRunner::pid`
- `CommandRunner::send_unix_signal`
- `CommandRunner::send_unix_sigterm`

## [0.2.0] - 06-06-2023
### Changes
- Forwarded `stacked_errors` and its features
- Moved ctrl-c functionality from `std_init` into its own function
- Made the entrypoint path optional for cases in which the container has a default entrypoint
- Reworked Container::new and added functions for `build_args` and `create_args`

## [0.1.0] - 29-05-2023
### Additions
- Initial release
