# Changelog

## [0.5.1] - 07-10-2023
### Fixes
- Fixed that failures on `ContainerNetwork` creation would result in panics
- Fixed some places where multiple termination could cause panics for `CommandRunners`

## [0.5.0] - 07-09-2023
### Changes
- `stacked_errors` 0.3.0
- Derived `Clone` for `CommandResult`
- Added `CommandResultNoDbg`
- Termination now will set the results it can for `Command`s and `ContainerNetwork`s
- Docker networks with `NetMessenger`s now have much cleaner errors

## [0.4.0] - 27-06-2023
### Changes
- Changed the semantics of `remove_files_in_dir` for hopefully the last time
- Refactored the way Dockerfiles are handled in `ContainerNetwork`s and in `Container`s
- `Command`s and `ContainerNetwork`s now produce no warnings on dropping if the thread is panicking
- Changed the result of `type_hash` to be 16 bytes

### Additions
- Command ci_mode debugs are colored

## [0.3.1] - 14-06-2023
### Fixes
- Fixed that `auto_exec_i` would try to terminate twice

## [0.3.0] - 13-06-2023
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
