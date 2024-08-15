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

## Cross compilation

Cross compilation on Windows is practically impossible (believe me, I have tried going down the
rabbit hole twice, just use WSL 2 or MSYS2 instead). The dockerfile entrypoint pattern examples that
use cross compilation will not work on straight Windows. However, it is possible in general to cross
compile inside a container and still keep build artifacts for fast recompilation by voluming
`CARGO_HOME`. An example of what this looks like is
```
if cfg!(windows) {
    Container::new(
            "builder",
            Dockerfile::contents(/* build container definition */),
        )
        .volume(
            home::cargo_home().stack()?.to_str().stack()?, // using the `home` crate
            "/root/.cargo", // if building in a typical Linux container
        )
        .volume(/* base directory */, "/needs_build")
        .workdir("/needs_build")
        .entrypoint_args(["sh", "-c", &format!("cargo -vV && {build_cmd}",)])
        .run(
            Some(/* dockerfiles_dir */),
            Duration::from_secs(3600),
            /* logs_dir */,
            true,
        )
        .await
        .stack()?
        .assert_success()
        .stack()?;
}
```
I would include functions to do this in `super_orchestrator` itself, but at this level we are just
making too many environmental assumptions. If the cargo version used locally and in the build
container are too incompatible, there may be problems. This ultimately must be automated per-repo
and the `dockerfile_entrypoint_pattern` is just a starter example.

## Docker inconsistencies

Docker is usually good with consistency and being able to run the same thing between different
environments, but there are a few things that have different defaults (meaning something that runs
perfectly on one environment may fail in another), most notably things involving network access.
- When enabling IPv6, you should use the arguments
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
- The "--internal" network argument does not have the intended effect on all platforms, even on some
  WSL 2 Linux distributions.
