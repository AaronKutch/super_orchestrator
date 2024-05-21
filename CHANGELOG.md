# Changelog

## [0.12.1] - 2024-05-20
### Fixes
- Fixed several minor issues with the `Command` recorder forwarding to stdout
- Fixed a mistake where a string intended as a format string was not actually in a `format!`

## [0.12.0] - 2024-04-22
### Fixes
- Partially fixed a long standing issue with containers not being stopped from CTRL-C/sigterm. The
  ctrl-c handler needs to be set in the right place for this to work (see the
  docker_entrypoint_pattern example).

## [0.11.0] - 2024-04-06
### Fixes
- Fixed path canonicalization on Windows to use `dunce::simplify` to avoid UNC paths

### Changes
- Replaced `auto_exec_i` with `auto_exec` which allows customizing arguments
- Removed the `log` dependency in favor of `tracing`
- Removed `std_init`

### Additions
- Added a `workdir` option to `Container`

## [0.10.0] - 2024-01-20
### Fixes
- Fixed compilation on Windows
- Fixed an issue with an example

### Changes
- Updated `env_logger` to 0.11
- Removed the "ctrlc_support" and "env_logger_support" features, their functionality is always
  enabled now
- Made "nix_support" not default, which improves usage on non-Unix targets

## [0.9.0] - 2023-11-11
### Fixes
- Fixed that `Command`s and all downstream constructs would add an extra newline byte at the end of
  standard stream copying even if there wasn't one in actuality
- Fixed debug outputs freezing if a newline did not come

### Changes
- Overhaul of many function signatures
- new `external_entrypoint` that volumes to the root of the container
- Updated dependecies, moved some into dev-dependencies
- Changed many things about how `Commands` handle debug and log files
- `CommandResultNoDebug` now has the stream data fields (still not including them in the debug impls)
  since they can be limited in several ways now
- `CommandResult::no_debug` now takes by value
- renamed `CommandResultNoDbg` to `CommandResultNoDebug`
- many other changes

## [0.8.0] - 2023-10-18
### Fixes
- Fixed that `no_uuid_for_host_name` was doing the opposite of what it was supposed to

### Additions
- Added environment variable args and some more methods to `Container`

## [0.7.0] - 2023-09-08
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
- Added `CommandResultNoDebug`
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
