# Hybrid execution implementation status

This file tracks implementation evidence without turning the requirements plan into a mutable checklist.

2026-09-09 u1 hardening: OAuth owner-secret failures are isolated per pending
authorization with a two-request verification concurrency cap; deployment
smokes validate TLS and bound backend-switch retries. Tenant provisioning is
serialized, preserves persistent salts, and avoids duplicate image imports.
The rootless tenant container is now supervised as a notifying, restarting
systemd service, and a forced container kill recovered successfully. Public
activation now retries after gateway restarts so the shared ngrok endpoint is
restored automatically. Fresh owner and generic-client u1 OAuth flows each
passed two refreshes and all five MCP tools. The remaining release follow-ups
are native Windows evidence and automatic tenant certificate renewal before
the documented day-75 manual rotation deadline.

2026-09-08 isolated-tenant MVP: a second stack named `u1` is live at
`/u1/mcp` beside the owner and Yandex Mail routes. It has a separate Linux
identity, rootless Podman graph/container, gateway/runner, OAuth database and
keys, workspace, persistent home, and an intentionally unconfigured `gh`
credential store. Cross-identity reads of the owner and `u1` homes failed, and
container mount inspection showed distinct workspace/home sources. The public
generic ChatGPT OAuth client completed consent, authorization-code exchange,
two refreshes, and all five MCP tool calls. The owner five-tool OAuth smoke was
rerun afterward and passed; owner, `u1`, router, Yandex/Docker dependencies all
remained active. This is an operator-provisioned VPS-only tenant, not the final
dynamic multi-user implementation and not a claim of Windows validation.

2026-09-07 verification follow-up: the focused native macOS Keychain enrollment
test passed outside the execution sandbox, including non-exportability, CSR,
certificate matching, and rustls signing. The prior two `Remote key error`
failures remain unexplained; a later pass does not establish their cause.
The CSR adapter now preserves the underlying macOS signing error instead of
discarding it. Descriptions correctly identify the implementation as a
non-exportable default-file-Keychain key, not a Secure Enclave key. Focused
`cargo check`, `cargo clippy -p mcp-agent --lib --offline -- -D warnings`, and
`git diff --check` passed. The complete Linux release gate now passes after
the repository-local skill changes; the resulting owner-mode release is
active on the VPS.
Focused contract, local-backend, skill-store, xtask, formatting, upstream
verification, and package-manifest tests pass in the restricted environment.
Native sandbox, relay socket, runtime process, and transport-spike tests cannot
bind or invoke macOS sandbox-exec here; the expanded execution request was
rejected by the host usage limit. The npm packaging path and native Keychain
test helper now place Swift and Clang module caches under writable build/test
directories for reproducible builds; this
environment still has an SDK/toolchain mismatch, so no native package artifact
is claimed.
The npm CLI tests themselves pass (2/2), and `cargo check -p xtask --offline`
passes after the packaging-cache adjustment.

The npm package smoke now uses an isolated temporary cache and `--offline`,
so local validation cannot be poisoned by a root-owned user cache or an
unrelated registry lookup. The CLI tests (2/2), packaged darwin-arm64 smoke,
documentation tests (4/4), formatting, and diff checks pass with that change.
The checkpoint, recovery, and privacy fixtures also pass locally; the storage
fixture correctly refuses to run without root and remains covered by the Linux
CI/VPS gate.
The full workspace clippy gate (`--all-targets --all-features -- -D warnings`)
also passes locally; only tests that invoke macOS sandbox-exec remain host-
restricted here.
The standalone skill-store suite passes 38 tests across catalog, cursor,
resource, system, and upstream contracts; the VPS runner target also builds
and its unit target has no additional cases.
All packaged POSIX scripts, the Python OAuth smoke, and npm launcher/smoke
JavaScript pass syntax checks; no stale release-gate wording remains in this
evidence file.

2026-09-07 owner-mode follow-up: the single-owner VPS container has an explicit
TCP/22 exception to the host's public IPv4 address. The setting is installed as
systemd drop-ins outside the active release, survived a runner restart, and an
in-container SSH attempt reached host-key verification. The opt-in scripts and
owner/multi-user boundary documentation are included in deterministic VPS
packaging; package reproducibility and documentation gates pass. Browser
inspection also confirmed that the pre-existing Yandex Mail MCP OAuth connection
is active and all six mail tools are visible in ChatGPT.
The owner container now also has a dedicated persistent SSH identity, a pinned
VPS host key, and a working `ssh vps` alias. An in-container command reached the
host as UID 0 and read both gateway and runner as active. The image records the
OpenSSH `/root/.ssh` to persistent-home linkage needed to preserve this after an
image replacement.

2026-09-07 Linux/VPS release follow-up: the Git-backed VPS build passed
formatting, workspace clippy, the complete workspace test suite, Linux native
sandbox (17/17), system catalog (7/7), upstream verification, OAuth
conformance, hybrid smoke, and reproducible package verification. The active
release is `/opt/tools-mcp/releases/20260907-owner-v5`; the prior release is
retained for rollback. Private and public OAuth smoke both returned
`oauth-refresh-and-five-tools-ok`, including two refreshes and the five-tool
matrix. Public discovery checks returned 200 for the tools-mcp and Yandex Mail
routes and 404 for an unmatched route; the Yandex Mail container remained
healthy. Port ownership is unchanged: Amnezia/Docker still owns public 443,
while tools-mcp remains on loopback 8443/8445 and the mTLS relay on 8444.
The owner-only `ssh vps` path was rechecked from the active container after
deployment. Native Windows CI/live ChatGPT, power-loss, external VPN,
private-GitHub push, and timed soak evidence remain outstanding.

Implementation override recorded 2026-09-06: live ChatGPT use showed that the
plan's deferred root-only project-skill rule made repository-local
`.agents/skills` invisible in the persistent multi-repository VPS workspace.
The user requested repository-local behavior. Project selection is therefore
session-scoped to the nearest Git ancestor of an accepted command `workdir`,
without recursively combining repositories or changing the five tool schemas.

| Unit | Status | Current evidence | Remaining release evidence |
|---|---|---|---|
| U1 | Complete | Passed ChatGPT checkpoint in `tests/e2e/chatgpt-scan-tools-checkpoint.toml`; stable free ngrok edge; five tools; repeated refresh; conversation correlation | Re-run during U10 live matrix |
| U2 | Complete | Execution-free contracts, local backend split, sealed launcher, dependency audit, fake-backend conformance | Re-run full release gates |
| U3 | In progress | Native restricted-token helper now uses `DISABLE_MAX_PRIVILEGE`, `LUA_TOKEN`, `WRITE_RESTRICTED`, a root-derived restricting SID, logon/World restricting SIDs, a medium-integrity ceiling, an explicit inherited-stdio allowlist, suspended launch, and a non-breakaway kill-on-close Job Object. The verified manifest records that enforcement contract. Native Windows tests cover workspace/outside writes, restricted token inspection, junction rejection, hard-link/rename and registry denial, inherited-handle exclusion, CNG isolation, descendant cleanup, PowerShell pipe/PTY/input/Ctrl-C/shutdown, package integrity, and CNG-backed enrollment. The formerly macOS-only conformance command now admits Windows, and `windows-latest` builds from a path containing spaces/non-ASCII text before running the workspace, relay, conformance, and package gates. Target-specific library checks pass for both `x86_64-pc-windows-gnu` and `x86_64-pc-windows-msvc`; full all-target native compilation remains a Windows-runner responsibility because the macOS host cannot provide the MSVC C toolchain. All locally executable authority/runtime tests pass. A platform-selecting npm launcher now persists one-time relay configuration, preserves the launch cwd for bare daily startup, builds deterministic target-specific tarballs, and installs the full native release in a clean-prefix smoke test. | Obtain the first native `windows-latest` result and fix any Windows-kernel-specific failure; publish the packages after npm registry authentication; manual Windows→ChatGPT remains an explicit post-release caveat |
| U4 | In progress | Bounded typed protocol, strict registration and manifest allowlist, fail-closed disposition ledger, pre/post-writer loss semantics, cancellation, tracked socket shutdown, strict reconnect epochs, handshake-enforced mTLS fixtures (valid, missing, wrong CA/EKU, expired, unknown, revoked, malformed), loopback WebSocket tests, idle p95 gate, a live gateway→mTLS relay→VPS runner five-tool smoke, and verified non-exportable macOS Keychain and Windows CNG rustls signers with certificate-substitution rejection | Obtain native Windows CNG/relay CI evidence and re-run relay gates in U10 |
| U5 | Complete | Newest-wins routing, strict same-launch reconnect epochs, permanent supersession across restart, four-second lease fencing, serialized route fences, terminal/skill affinity, durable non-reuse watermark, bounded fail-closed tombstones, replaceable/fenceable VPS generations, explicit no-backend behavior, takeover/partition/loss races, and allocator/restore tests. Relay protocol v2 now carries bounded authenticated principal/session fingerprints to the worker, enabling session-scoped active-project selection without changing tool arguments. | Re-run gateway and hybrid release gates in U10 |
| U6 | In progress | OAuth discovery, consent/denial, PKCE, CSRF/browser headers, bearer middleware, HMAC-keyed durable codes/tokens with no-reauthorization legacy refresh migration, serialized one-winner code/refresh races with family-wide replay revocation, durable per-consent grant authority, grant-bound MCP identities/handles, live in-flight cancellation plus explicit native terminal process-tree termination on grant revoke, owner-secret throttling, active device-revocation socket closure, monotonic security revision, matched-vs-stale recovery tooling, and an SSH-only root-issuer CSR ceremony. Packaged macOS and Windows enrollment paths create non-exportable P-256 keys in Keychain/CNG, produce signed PKCS#10 CSRs, reject certificate substitution, and sign through rustls without materializing private-key files. An isolated Linux fixture passed matched restore, stale credential/device invalidation, allocator rollback exclusion, and checksum-tamper rejection on the VPS without touching live state. | Obtain native Windows enrollment evidence and re-run the complete OAuth persistence/revocation suite |
| U7 | In progress | A rebuilt digest-pinned Ubuntu 26.04 image is live under rootless Podman with private namespaces, the six file/UID capabilities required for in-container package management and no system/network capabilities, no engine sockets or published ports, enforced 1.5-CPU/2.5-GiB/512-PID/4096-FD limits, independent bounded graph/workspace/home ext4 volumes, and host-private egress denial. Exact versions for every R16 tool were recorded. `tree`, workspace, and home survived ordinary restart; private/link-local/host probes failed while DNS/HTTPS and an internal-only listener worked. Byte/inode fixtures isolated ENOSPC; live PID and OOM limits fired while 240/240 gateway probes remained healthy at 59 ms maximum; a detached marked descendant was reaped. Human `gh auth login` state is present in persistent home; after connecting its credential helper with `gh auth setup-git`, authenticated fetch from the private Yandex Mail repository passed. | Complete the private clone/commit/push fixture; retain the previous image/container through soak and re-run containment in U10 |
| U8 | In progress | Versioned release and atomic recovery-set backup are live. Gateway uses loopback TLS `8443`, relay uses public mandatory-mTLS `8444`, and one VPS ngrok agent targets an exact-path HAProxy router on loopback `8000`. A real reboot changed the boot ID while preserving workspace/home hashes, SQLite, service identities, and every Docker container ID/state. Recovery exposed two inherited Podman pause-namespace defects; the supervisor now admits only TUN, leaves nested `/proc` and `/tmp` mounts available, and proves both before start. Post-fix all services, exact Yandex/tools/unmatched routes, SSH, containment, and 443 ownership passed while Docker/Amnezia remained unchanged. | Complete abrupt power-loss, interrupted activation, matched rollback, external VPN, and certificate-renewal drills |
| U9 | In progress | Hybrid routing, session-scoped repository-local project skills, Windows caveat, VPS topology/route ownership, checkpointed upgrades, three restore classes, GitHub trust boundary, and +5m/+1h/+24h/+7d operator gates are documented. Privacy-safe structured gateway metrics, a fixed host-metric allowlist, root-only atomic collection, expiring external-probe evidence, core-dump denial, canary privacy scans, and recovery-set coverage for interpreting configuration have deterministic tests. The live collector reports healthy SQLite, route, Docker/Amnezia baseline, containment and 1 ms gateway p95; seeded live privacy scan passed. | Obtain external VPN evidence and complete the +1h/+24h/+7d timed operator drills |
| U10 | In progress | OAuth and hybrid smoke commands cover auth/refresh, routing, relay, backend boundary, and a routed five-tool matrix. macOS/Windows/Linux CI invokes the applicable hybrid gates. A separate Linux VPS bundle records provenance, modes and checksums, rejects unmanifested/tampered assets, and reproduces byte-for-byte. The deployed `vps-smoke` gate passed storage, rootless containment, container, recovery, and live privacy suites. Repository-local skill regressions prove nearest-Git-root selection, MCP-session separation, stale-handle denial, no-follow traversal, and authenticated identity propagation across relay protocol v2. The 2026-09-07 Linux/VPS release follow-up reran the complete Linux gate and public/private OAuth five-tool smokes against the active owner release. Evidence-scoped release notes distinguish observed macOS/VPS behavior from pending Windows/live gates. | Native Windows CI/package, live ChatGPT reconnect/switching, power-loss/external-VPN/GitHub drills, and seven-day soak |

Last deterministic gate: on the VPS build, `cargo fmt --all -- --check`,
workspace clippy with `-D warnings`, and the complete workspace test suite
passed on 2026-09-07. The same release passed Linux sandbox, system catalog,
upstream, OAuth-conformance, hybrid-smoke, and reproducible-package gates;
private and public OAuth five-tool smokes passed against the active release.
