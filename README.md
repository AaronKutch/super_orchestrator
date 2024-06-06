# Super Orchestrator

The purpose of Super Orchestrator is to act as a more easliy programmable, scalable, and debuggable
alternative to the horrors of bash scripts and `docker-compose`. This is based on Tokio and supplies
convenient tools for file management, command running, and Docker container management.

First, see the documentation of `stacked_errors`
(https://docs.rs/stacked_errors/latest/stacked_errors/) to understand the error strategy. Then, look
over the documentation. Finally, check the examples in order of: paths, file_options,
basic_commands, basic_containers, commands, dockerfile_entrypoint_pattern, postgres, and clean.

Note that Windows has several intrinsic issues such as cross compilation being a pain (the
dockerfile entrypoint pattern will not work without a lot of setup). Any of the examples with
UNIX specific commands will of course not work.

The "nix_support" feature enables some functions to be able to send UNIX signals to commands.
