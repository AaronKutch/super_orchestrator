# Super Orchestrator

The purpose of Super Orchestrator is to act as a more easliy programmable, scalable, and debuggable
alternative to the horrors of bash scripts and `docker-compose`.

First, see the documentation of `stacked_errors`
(https://docs.rs/stacked_errors/latest/stacked_errors/) to understand the error strategy.

The main useful constructs of `super_orchestrator` are `Containers`s, `ContainerNetwork`s,
`Dockerfile`, `Command`, and `FileOptions`
