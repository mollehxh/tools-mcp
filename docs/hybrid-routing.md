# Hybrid routing contract

One ChatGPT MCP app uses
`https://fidela-unsubversive-imaginarily.ngrok-free.dev/mcp`. OAuth, routing,
and public handle ownership live in the VPS gateway; neither backend receives
OAuth credentials or routing fields as tool arguments.

The model-visible surface is exactly `exec_command`, `write_stdin`,
`apply_patch`, `skills.list`, and `skills.read`.

## Backend selection

- With no eligible local lease, new calls use the persistent Linux VPS
  container.
- Starting the packaged macOS or Windows command fixes its launch directory,
  opens an outbound mutually authenticated relay, and makes that launch the
  current local generation. No inbound local port is opened.
- A newer launch permanently supersedes the prior launch. The reachable old
  process exits immediately; a partitioned process self-fences within five
  seconds and cannot reclaim routing by reconnecting.
- If the local lease disappears, only later calls can fall back to the VPS.
  Written or possibly written work is never replayed.

Before a conversation executes against a generation different from its last
confirmed generation, the gateway returns one non-executing
`backend_changed`. Retry explicitly after checking the reported backend kind,
opaque workspace identity, operating system, generation, and privilege
posture. Concurrent calls remain fenced until that retry.

Terminal IDs, skill cursors, and mutable skill handles retain generation
affinity. A handle from a lost generation returns a lost-context error; it is
never resolved against the replacement backend. After a gateway or VPS reboot,
all pre-reboot live terminal handles are lost even though files and grants are
durable.

Project-skill selection is also session-scoped. After a successful
`exec_command` with an explicit `workdir`, the worker selects the nearest Git
ancestor and serves project skills only from that repository's
`.agents/skills`. The authenticated principal and MCP-session fingerprints
cross the relay as bounded protocol metadata, never as tool arguments. Sibling
repositories are not scanned or merged, and handles from a previously selected
repository fail closed after a switch.

## Workspace continuity

Local and VPS files never synchronize automatically. The local launch works in
the directory from which it was started. VPS calls use explicit paths below
the persistent multi-repository `/workspace`. Move work only through explicit
Git commits, fetches, pulls, and pushes.

The VPS container runs commands as container root. It can install packages and
read its own persistent GitHub CLI credential, but it has no host root,
container-engine socket, relay key, host filesystem, or host/private network
access. Treat a route change as a real change of machine and checkout.

## Loss outcomes

- `not_dispatched` means authenticated execution rejected the request or the
  relay was lost before writer handoff; an explicit retry is safe.
- `outcome_unknown` means the request may have reached a worker. Inspect state
  before deciding whether to issue a new command.
- `backend_changed` performs no tool effect and asks for one explicit retry.
- `backend_lost` on an affine handle is terminal for that handle.
- `no_backend` means neither a verified VPS runner nor a healthy current local
  lease exists. It fails closed; no host command or implicit retry occurs.

OAuth refresh is automatic after one consent. Routine gateway restarts do not
require consent again. Device enrollment/revocation, OAuth revocation, GitHub
login or scope changes, and VPS administration remain human-only operations.
