# Installing tools-mcp local workers

The first release ships native local-worker artifacts for macOS and Windows.
Linux local-worker packaging remains deferred; Linux gateway and VPS-runner
artifacts are separate. See `windows-installation.md` for the native Windows
boundary and its explicit live-test caveat, `hybrid-routing.md` for automatic
local/VPS selection, and `vps-deployment.md` for the server topology.

## Install the local command

The supported user-facing installation is the npm package. It selects the
matching native macOS or Windows artifact; Node is only the launcher and does
not execute tools itself:

```sh
npm install --global tools-mcp
```

After the one-time device enrollment below, save the relay configuration once:

```sh
tools-mcp setup \
  --relay-url wss://178.215.236.207:8444/relay \
  --relay-ca "$HOME/.config/tools-mcp/relay-ca.pem" \
  --device-cert "$HOME/.config/tools-mcp/laptop-main.pem" \
  --device-key "$HOME/.config/tools-mcp/laptop-main.key" \
  --device-id laptop-main
```

Daily startup is deliberately a bare command. The current directory becomes
the immutable workspace for that launch:

```sh
cd ~/dev/some-project
tools-mcp
```

Configuration is stored with user-only permissions under
`~/Library/Application Support/tools-mcp/config.json` on macOS or
`%APPDATA%\\tools-mcp\\config.json` on Windows. A configured PEM key must have
mode `0600`; only its absolute path is stored in the config. Omitting
`--device-key` retains the platform key-store flow, where the private device key remains
non-exportable through Keychain/CNG.

## Build a native package from source

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

## One-time hybrid enrollment and daily startup

Enrollment is a human-controlled local-terminal plus VPS-SSH ceremony. It is
not an MCP tool and must never be delegated to a model call. On the local
computer, choose a portable device ID and create a CSR backed by the platform's
non-exportable key store:

```sh
tools-mcp enroll-device --device-id laptop-main --csr-output laptop-main.csr
openssl req -in laptop-main.csr -outform DER | shasum -a 256
```

Copy only the CSR to the VPS. In an SSH session, independently inspect the CSR
subject, public-key fingerprint, signature, requested device ID and platform;
then run `tools-mcp-admin approve-device-csr` with an expiry no more than 90
days away. Copy the resulting certificate chain and the public relay CA back to
the local computer and validate/install it:

```sh
tools-mcp install-device-certificate \
  --device-id laptop-main \
  --device-cert laptop-main.pem
```

The native binary still accepts explicit relay arguments for diagnostics and
source builds. On macOS the key source may be
`--device-key /absolute/path/to/laptop-main.key` to avoid Keychain prompts, or
`--device-keychain-label tools-mcp-device:laptop-main` for a non-exportable
key. On Windows it is `--device-cng-key-name tools-mcp-device:laptop-main`:

```sh
/absolute/path/to/mcp-agent \
  --relay-url wss://178.215.236.207:8444/relay \
  --relay-ca /absolute/path/to/relay-ca.pem \
  --device-cert /absolute/path/to/laptop-main.pem \
  --device-key /absolute/path/to/laptop-main.key \
  --device-id laptop-main
```

The newest successful launch becomes current and immediately fences the old
launch. If no enrolled local command is healthy, new calls use the VPS. A
`backend_changed` response is intentionally non-executing: inspect its opaque
context and retry once. `no_backend` means neither local nor verified VPS
execution is eligible; do not fall back to host execution.

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
  --dest "/absolute/path/to/repository/.agents/skills"
```

For a routed multi-repository workspace, an accepted `exec_command` with an
explicit `workdir` selects the nearest Git ancestor inside that MCP session.
Later `skills.list` and `skills.read` project-scope calls use that repository's
`.agents/skills`. The launch workspace remains the default until a repository
is selected. Discovery never recursively combines sibling repositories, and a
project handle or cursor fails closed after that session selects another
repository. Global and system skills are unaffected.

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
