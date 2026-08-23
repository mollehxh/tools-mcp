# macOS security model

The shipped backend is macOS Seatbelt. Linux and Windows sandboxing and
packaging are deferred and unsupported by this release. `/usr/bin/sandbox-exec`
is a required, deprecated macOS facility, so future macOS releases are not
supported until tested.

## Direct capability boundary

At startup, `mcp-agent` fixes a canonical workspace and canonical managed roots,
verifies the complete package, and runs native positive and negative probes.
Every `exec_command` process and direct descendant receives the same policy:

- host files readable by the macOS account are readable by the workload;
- direct writes are allowed below the workspace, including `.git`, `.codex`,
  `.mcp-agent`, `.agents/skills`, caches, and generated files;
- direct writes are also allowed below canonical `/tmp`, canonical `$TMPDIR`,
  `$CODEX_HOME/skills`, the effective `CARGO_HOME` and `GRADLE_USER_HOME`, and
  `$CODEX_HOME/cache/tools-mcp/workspaces/<workspace-hash>`;
- cache-only variables for XDG, Cargo targets, npm, Yarn, pnpm, pip, uv, and Go
  are steered into that workspace-partitioned tools-mcp cache;
- outbound connections, host-local connections, inbound connections, and
  listener binds on loopback, wildcard, or non-loopback interfaces are allowed;
- available child processes may run, and direct descendants inherit this
  filesystem and network boundary.

No other home or system path is directly writable by default. `apply_patch` is
separately confined to the canonical workspace, but it does not protect
workspace metadata. `skills.list` and `skills.read` can read only registered,
bounded system, project, and global skill packages. The release-owned system
skill root is manifest-verified, must not overlap a writable root, and is
revalidated when a system skill is listed or read.

The persistent Cargo and Gradle homes are writable because normal toolchains
need their existing registries and configuration. This also means a workload
can durably modify Cargo configuration, credentials, registries or executables,
and Gradle init scripts, plugins, caches or executables. Global skills under
`$CODEX_HOME/skills` are durable executable or instructional state. Shared
`/tmp`, `$TMPDIR`, cache, and tool-state roots are not private workspace
storage: aliases or state created by another same-user process may already be
present.

## Exposure and non-goals

Possession of an active tunnel URL grants remote command execution with all of
the capabilities above: host-readable data, declared writable roots, durable
global-skill and tool-state modification, unrestricted workload networking, and
listener binds. The MCP endpoint itself remains bound to loopback; an external
tunnel forwards to it. The URL is authority, not harmless connection metadata.

Skills fetched from GitHub are unreviewed third-party instructions and may
include executable content. Installing a skill does not review or sandbox it
more narrowly; when an agent later follows it, its commands receive the ordinary
workload capabilities. Review the source and pin before use.

Seatbelt contains direct filesystem writes by the workload process tree. It does
not contain effects brokered through outbound APIs, `launchctl`, Docker,
databases, credential-bearing developer tools, local agents, or other services
available to the account. It does not protect readable secrets from commands or
network exfiltration. It is not authentication, multi-user isolation,
denial-of-service protection, or whole-machine/VPS containment.

Commands are automatic and expose no per-command escalation argument. Before
serving, startup verifies release files and modes, root separation, the native
launcher, writable-root probes, descendant inheritance, and denial against a
release canary. A denial or setup failure is never retried unsandboxed.
