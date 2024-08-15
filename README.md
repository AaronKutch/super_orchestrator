# Super Orchestrator

The purpose of Super Orchestrator is to act as a more easliy programmable, scalable, and debuggable
alternative to the horrors of bash scripts and `docker-compose`. This is based on Tokio and supplies
convenient tools for file management, command running, and Docker container management.

First, see the documentation of `stacked_errors`
(https://docs.rs/stacked_errors/latest/stacked_errors/) to understand the error strategy. Then, look
over the documentation. Finally, check the examples in order of: paths, file_options,
basic_commands, basic_containers, commands, dockerfile_entrypoint_pattern, postgres, and clean 
(note that some of these use UNIX specific commands that will not run successfully in some
environments).

# Notes

The "nix_support" feature enables some functions to be able to send UNIX signals to commands.

Cross compilation on Windows is practically impossible (believe me, I have tried going down the
rabbit hole twice, just use WSL 2 or MSYS2 instead). The dockerfile entrypoint pattern has a
workaround for this that builds on straight windows by cross compiling within a container while
retaining build artifacts (but be aware that this may break if the cargo version in the container
is not up to date).

Docker is usually good with consistency and being able to run the same thing between different
environments, but there are a few things that have different defaults, most notably things involving
network access. When enabling IPv6, you should use the arguments
```
container.create_args([
    "--cap-add=NET_ADMIN", // if doing anything requiring admin access for network stuff
    "--sysctl",
    "net.ipv6.conf.all.disable_ipv6=0", // some environments need this also
    "--sysctl",
    "net.ipv6.conf.default.disable_ipv6=0", // some environments need this also
    "--sysctl",
    "net.ipv6.conf.all.forwarding=1", // for packet forwarding if needed
])
```
