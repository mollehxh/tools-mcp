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

Keep the complete extracted directory together, including the executable,
manifests, `sandbox/`, notices, and `system-skills/skill-installer/`. The binary
resolves this compatibility set relative to its own installed path; it never
searches `PATH` for policy assets or for `/usr/bin/sandbox-exec`. Startup
verifies exact files, modes, version, target, protocol, and checksums, then
executes a native read/write/network self-test before serving.

From the project to expose, run the packaged binary using an absolute path:

```sh
/absolute/path/mcp-agent-0.1.0-aarch64-apple-darwin/mcp-agent
```

The workspace is fixed to the launch directory for the lifetime of the
process. Source-build tests may use `--release-dir`; that override is a
development seam and is not the installed-package workflow.

## Tool and skill workflow

The server exposes exactly five model-visible tools:

- `exec_command`
- `write_stdin`
- `apply_patch`
- `skills.list`
- `skills.read`

There is no `skills.install` RPC. On first startup, `skills.list` with scope
`system` discovers the immutable built-in package at
`skill://host/system/skill-installer/SKILL.md`. Read that exact resource with
`skills.read`, then run its original Python entry point through `exec_command`.

Install globally into `$CODEX_HOME/skills` (the script default; `$CODEX_HOME`
defaults to `~/.codex`):

```sh
python3 "$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py" \
  --repo OWNER/REPOSITORY \
  --path path/to/skill
```

Install into the selected project with the original `--dest` option:

```sh
python3 "$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py" \
  --repo OWNER/REPOSITORY \
  --path path/to/skill \
  --dest "/absolute/path/to/workspace/.agents/skills"
```

The script also accepts a GitHub tree `--url`, multiple `--path` values,
`--ref`, and `--method auto|download|git`. It preserves upstream behavior:
public download, `GITHUB_TOKEN`/`GH_TOKEN`, HTTPS then SSH Git fallback,
collision refusal, temporary cleanup, and non-transactional partial results
when a multi-path run fails after an earlier installation. The MCP server does
not prevalidate, roll back, or postprocess the command result. A later
`skills.list`/`skills.read` observes successful global or project changes
without a server restart.

Remote skills are unreviewed third-party instructions or executable content.
Review and pin their source before using them; their commands receive the full
workload authority described in `docs/security-model.md`.

Validate the packaged loopback server with the pinned Inspector:

```sh
cargo run -p xtask -- inspector-smoke
```
