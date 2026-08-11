# Deferred U2 native sandbox work

Recorded on 2026-08-11 for branch `feat/mcp-agent-foundation`.

## Status

U2 is verified locally on macOS, including real Seatbelt enforcement, mandatory
preflight, outside-write denial, protected-root denial, child inheritance,
loopback access, and managed-root race coverage.

U2 is not formally complete under the plan's cross-platform Definition of Done.
Development of dependent units may continue on macOS, but the project must not
claim Linux or Windows sandbox support until the items below are resolved.

## Deferred work

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

## Required final gate

Before U2 is marked complete, run the shared clean-install conformance suite on
native macOS, Linux, and Windows with no skipped sandbox case and unchanged
outside sentinels.
