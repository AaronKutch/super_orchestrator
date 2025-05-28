# Super Orchestrator

The purpose of Super Orchestrator is to act as a more easliy programmable, scalable, and debuggable
alternative to the horrors of bash scripts and `docker-compose`. This is based on Tokio and supplies
convenient tools for file management, command running, and Docker container management.

First, see the documentation of `stacked_errors`
(https://docs.rs/stacked_errors/latest/stacked_errors/) to understand the error strategy. Then, look
over the documentation. Finally, check the examples (run the testcrate binaries from the root of the
workspace via `cargo run --bin ...`) in order of: paths, file_options, basic_commands,
basic_containers, commands, dockerfile_entrypoint_pattern, postgres, and clean 
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
Container::new("builder", Dockerfile::contents(/* build container definition */))
    // volume in cargo's registry using the `home` crate
    .volume(
        home::cargo_home()
            .stack()?
            .join("registry")
            .to_string_lossy(),
        "/root/.cargo/registry",
    )
    .volume(cwd.to_string_lossy(), "/app")
    .workdir("/app")
    // Use a target directry separate from the main one so that it doesn't conflict,
    // note there is a rust-analyzer setting to do a similar thing. Also, if building
    // multiple binaries it is better to pass them all to the same call.
    .entrypoint(
        "cargo",
        ["build", "--release", "--target-dir", "target/isolated"]
            .into_iter()
            .chain(bins.iter().flat_map(|bin| ["--bin", bin]))
            .chain(features.iter().flat_map(|feature| ["--feature", feature])),
    )
    // where there should be a dockerfiles directory and logs directory under `cwd`
    .run(
        Some(&cwd.join("dockerfiles").to_string_lossy()),
        Duration::from_secs(3600),
        &cwd.join("logs").to_string_lossy(),
        true,
    )
    .await
    .stack()?
    .assert_success()
    .stack()?;
```
I would include functions to do this in `super_orchestrator` itself, but at this level we are just
making too many environmental assumptions. If the cargo version used locally and in the build
container are too incompatible, there may be problems. This ultimately must be automated per-repo
and the `dockerfile_entrypoint_pattern` is just a starter example.

`x86_64-unknown-linux-musl` on an Alpine container is the best way to cross compile, because MUSL is
static and the same binary can usually work in any environment supporting it. This means that, with
the right setup, you can compile in release mode once and be able to run it locally, but also have
containers volume the binary into themselves to also run at the same time. As of writing however, if
you have dependencies involving the `aws-lc-sys` crate (with its dependent `aws-lc-rs` being a
replacement for the `ring` crate), it is impossible to use MUSL because it does not have the
necessary symbols yet. `x86_64-unknown-linux-gnu` and other targets needing dynamic compilation are
very difficult to get working in the same way as MUSL. You may need to use the same trick as the
workaround for Windows above, where you compile in the same base container that will be used later
for running the binary, ensuring the binary has the correct links.

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
