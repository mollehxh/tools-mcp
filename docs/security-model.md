# Security model

The first release has three execution boundaries: macOS Seatbelt, a Windows
restricted-token/Job Object helper, and a rootless Podman VPS workload. Linux
local-worker packaging remains deferred. `/usr/bin/sandbox-exec` is a required,
deprecated macOS facility, so future macOS releases are not supported until
tested. The unavailable manual Windows-to-ChatGPT walkthrough is not claimed;
native Windows CI is the current evidence.

## macOS direct capability boundary

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

In a multi-repository workspace, project-skill reads are rooted through
no-follow directory capabilities at the nearest Git ancestor selected by that
MCP session's accepted command `workdir`. Session/project context is bounded;
it is not inferred through symlinks, mixed recursively across repositories, or
accepted from model-visible skill arguments. Switching repositories invalidates
the previous project context and its mutable handles.

The persistent Cargo and Gradle homes are writable because normal toolchains
need their existing registries and configuration. This also means a workload
can durably modify Cargo configuration, credentials, registries or executables,
and Gradle init scripts, plugins, caches or executables. Global skills under
`$CODEX_HOME/skills` are durable executable or instructional state. Shared
`/tmp`, `$TMPDIR`, cache, and tool-state roots are not private workspace
storage: aliases or state created by another same-user process may already be
present.

## Windows and VPS boundaries

The Windows helper is package-verified before relay registration. It launches
with a restricted token and medium-integrity ceiling, limits inherited handles
to declared stdio, confines writes to declared roots, assigns the suspended
child to a non-breakaway kill-on-close Job Object, and fails closed if CNG,
token, ACL, or Job Object enforcement is unavailable. Its enrollment key is
non-exportable in CNG.

The VPS runner and relay credential stay on the unprivileged host account,
outside container mounts. Commands run as UID 0 only inside a rootless user
namespace. The container has only the file/UID capabilities needed for package
management, private namespaces, no engine socket, no devices or published
ports, fixed cgroup and descriptor limits, and host-enforced denial of host,
private, link-local, metadata, Docker, Amnezia, and control networks. Ordinary
DNS/HTTPS/Git traffic and namespace-local listeners remain available.

The persistent GitHub CLI credential is deliberately readable by arbitrary
container-root commands. Use narrow repository scopes and do not describe it
as secret from the model. Gateway OAuth secrets, CA keys, relay keys, and host
control credentials are not mounted into the container.

## Exposure and non-goals

The local development endpoint remains loopback-only. The public hybrid
endpoint is not authorized by URL possession: the gateway requires OAuth and
the ngrok domain reaches it only through an exact-path loopback router. OAuth
does not reduce the authority of an accepted tool call; it protects who may
submit one. A compromised bearer/refresh family or enrolled device remains
sensitive and must be revoked.

Skills fetched from GitHub are unreviewed third-party instructions and may
include executable content. Installing a skill does not review or sandbox it
more narrowly; when an agent later follows it, its commands receive the ordinary
workload capabilities. Review the source and pin before use.

Each platform boundary contains only the capabilities stated above. It does not
contain effects brokered through allowed outbound APIs or credential-bearing
developer tools and does not protect readable workload secrets from network
exfiltration. The first release is single-user; it is not multi-user isolation.

Commands are automatic and expose no per-command escalation argument. Before
serving, startup verifies release files and modes, root separation, the native
launcher, writable-root probes, descendant inheritance, and denial against a
release canary. A denial or setup failure is never retried unsandboxed.
