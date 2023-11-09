# Super Orchestrator

The purpose of Super Orchestrator is to act as a more easliy programmable, scalable, and debuggable
alternative to the horrors of bash scripts and `docker-compose`. This is based on Tokio and supplies
convenient tools for file management, command running, and Docker container management.

First, see the documentation of `stacked_errors`
(https://docs.rs/stacked_errors/latest/stacked_errors/) to understand the error strategy. Then, look
over the documentation. Finally, check the examples in order of: paths, basic_commands,
basic_containers, commands, dockerfile_entrypoint_pattern, postgres, and clean
