# Protocol v2: Codex-parity skills and macOS capabilities

Protocol v2 exposes exactly five tools: `exec_command`, `write_stdin`,
`apply_patch`, `skills.list`, and `skills.read`. It removes the brokered
`skills.install` RPC and its Rust fetch, staging, receipt, and lifecycle code.

The release now includes an immutable, manifest-verified system
`skill-installer`. Agents discover and lazily read it through the system skill
origin, then run the pinned upstream Python installer as an ordinary command.
The default destination is `$CODEX_HOME/skills`; project installation uses
`--dest <workspace>/.agents/skills`. Project remains preferred over global for
ordinary name collisions, while the canonical `skill-installer` name selects
the system package and every exact origin remains addressable.

The macOS Seatbelt profile now favors ordinary development: the workspace
(including metadata), canonical temp roots, workspace-partitioned cache,
global skills, and effective Cargo/Gradle homes are writable. Host-readable
files, child processes, outbound and inbound networking, and workload listener
binds are allowed. Direct writes outside those fixed roots remain denied for
the workload process tree, and startup fails closed. Linux and Windows remain
unsupported.

Automated protocol-v2 conformance, packaged installer workflows, provenance,
deterministic packaging, and Inspector smoke are release gates. A fresh
ChatGPT Developer Mode/ngrok five-tool scan is separately recorded in
`tests/e2e/chatgpt-scan-tools-checkpoint.toml`; its pending status is a manual
release-signoff blocker and is not represented as automated proof.
