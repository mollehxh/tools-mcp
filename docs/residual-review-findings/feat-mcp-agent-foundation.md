# Superseded native sandbox findings

Recorded on 2026-08-11 for branch `feat/mcp-agent-foundation`; superseded on
2026-08-23 by the macOS-only capability contract in
`docs/plans/2026-08-23-1219-refactor-codex-parity-skills-sandbox-plan.md`.

## Status

The earlier protected-metadata and cross-platform Definition of Done is no
longer active. The shipped support contract is macOS only. Workspace metadata
is writable, managed temp/cache/tool-state roots are also writable, workload
networking and listener binds are unrestricted, and direct writes outside the
declared roots remain denied.

## Historical deferred work

The sections below are retained as historical context only. They are not
release blockers for the macOS package and must not be read as implemented or
supported behavior.

### Linux protected-create lifecycle

The current Bubblewrap command re-applies read-only mounts only for protected
roots that already exist. Initially absent `.git` and `.codex` roots therefore
still require a packaged, release-relative launcher lifecycle equivalent to the
pinned Codex `ProtectedCreateTarget` monitor. A simple `bwrap --dir` placeholder
is not sufficient because it can change host workspace and Git semantics.

Exit criteria:

- Package and authenticate the Linux launcher/helper relative to the release.
- Deny creation or replacement of initially absent protected roots without
  leaving host metadata behind.
- Prove outside-write denial, child inheritance, helper replacement resistance,
  and protected-create cleanup on a native Linux runner.

### Windows restricted-token helper

The authority layer currently defines a fail-closed protocol for an external
Windows helper, but the repository does not yet build and package the required
pinned restricted-token/elevated backend.

Exit criteria:

- Build and package a source-controlled Windows helper compatible with the
  pinned Codex backend and release manifest protocol.
- Hold the verified helper against replacement and enforce the workspace write
  boundary for descendants, reparse points, and inherited handles.
- Prove clean-install preflight, process-tree enforcement, helper absence, and
  helper replacement behavior on a native Windows runner.

## Historical final gate

The former three-platform gate is superseded. Any future Linux or Windows work
requires a new product contract, implementation, packaging, native conformance,
and CI proof before support can be claimed.
