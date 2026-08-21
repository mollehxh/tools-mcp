# Installing mcp-agent on macOS

This release supports macOS only. Linux and Windows packaging and native
sandbox claims are deferred; `xtask package` fails instead of emitting an
artifact on those hosts.

## Build a native package

Install the pinned Rust toolchain from `rust-toolchain.toml`, then run:

```sh
cargo run -p xtask -- package
```

The command builds the native `mcp-agent`, assembles the Seatbelt policy and
marker beside it, copies the Apache/NOTICE/third-party material, and writes a
deterministic archive under `target/release-artifacts/`. The directory and
archive names include the version and native target (`aarch64-apple-darwin` or
`x86_64-apple-darwin`). Re-running the command replaces only that exact output
and produces identical bytes when all inputs are identical.

Verify and unpack a downloaded archive before running it:

```sh
shasum -a 256 -c mcp-agent-0.1.0-aarch64-apple-darwin.tar.gz.sha256
tar -xzf mcp-agent-0.1.0-aarch64-apple-darwin.tar.gz
cd mcp-agent-0.1.0-aarch64-apple-darwin
shasum -a 256 -c SHA256SUMS
```

Keep the executable, `release-manifest.json`, `sandbox-manifest.json`, and the
`sandbox/` directory together. The binary resolves this compatibility set
relative to its own installed path; it never searches `PATH` for policy assets
or for `/usr/bin/sandbox-exec`. Startup verifies version, target, protocol, and
checksums, then executes a native read/write/network self-test before serving.

From the project to expose, run the packaged binary using an absolute path:

```sh
/absolute/path/mcp-agent-0.1.0-aarch64-apple-darwin/mcp-agent
```

The workspace is fixed to the launch directory for the lifetime of the
process. Source-build tests may use `--release-dir`; that override is a
development seam and is not the installed-package workflow.

Validate the packaged loopback server with the pinned Inspector:

```sh
cargo run -p xtask -- inspector-smoke
```
