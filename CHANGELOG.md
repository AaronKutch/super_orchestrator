# Changelog

## [0.7.0] - TODO
### Fixes
- Fixed that `ContainerNetwork`s were using the `name` for hostnames instead of the `host_name` that
  was meant for that purpose
- `Command` stdout copiers no longer panic on invalid utf-8

### Changes
- `Command` and `CommandResult` stdout and stderr are now `Vec<u8>` instead of `String`
- `ContainerNetwork` now adds on a UUID suffix to docker names and hostnames in order to allow
  running them in parallel
- there are no more `track_caller` functions, use `stacked_errors`
- Many dependency updates, use `postcard` internally instead of `bincode`

### Additions
- Added `CommandResult::stdout_as_utf8` and some other related functions for convenience
- Added `ContainerNetwork::terminate_containers` which just terminates containers and not the
  network

## [0.6.0] - 2023-10-07
### Changes
- `stacked_errors` 0.4.0, and removal of several now unnecessary feature flags
- tweaks to error outputs
- Use `serde` and `bincode` for `NetMessage` for now

## [0.5.1] - 2023-10-07
### Fixes
- Fixed that failures on `ContainerNetwork` creation would result in panics
- Fixed some places where multiple termination could cause panics for `CommandRunners`

## [0.5.0] - 2023-09-07
### Changes
- `stacked_errors` 0.3.0
- Derived `Clone` for `CommandResult`
- Added `CommandResultNoDbg`
- Termination now will set the results it can for `Command`s and `ContainerNetwork`s
- Docker networks with `NetMessenger`s now have much cleaner errors

## [0.4.0] - 2023-06-27
### Changes
- Changed the semantics of `remove_files_in_dir` for hopefully the last time
- Refactored the way Dockerfiles are handled in `ContainerNetwork`s and in `Container`s
- `Command`s and `ContainerNetwork`s now produce no warnings on dropping if the thread is panicking
- Changed the result of `type_hash` to be 16 bytes

### Additions
- Command ci_mode debugs are colored

## [0.3.1] - 2023-06-14
### Fixes
- Fixed that `auto_exec_i` would try to terminate twice

## [0.3.0] - 2023-06-13
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

## [0.2.0] - 2023-06-06
### Changes
- Forwarded `stacked_errors` and its features
- Moved ctrl-c functionality from `std_init` into its own function
- Made the entrypoint path optional for cases in which the container has a default entrypoint
- Reworked Container::new and added functions for `build_args` and `create_args`

## [0.1.0] - 2023-05-29
### Additions
- Initial release
