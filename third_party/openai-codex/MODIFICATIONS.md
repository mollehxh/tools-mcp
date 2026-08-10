# OpenAI Codex compatibility modifications

Pinned upstream commit: `8cabf5a6cf103cebe338d46346e43e3201e64f41`.

Files classified `unchanged` in `SOURCE.toml` are byte-identical. Local adapters live outside
the upstream subtrees. The frozen MCP fixtures intentionally adapt the pinned contracts as follows:

- `apply_patch` transports the original freeform patch through one MCP object field (R6).
- approval/escalation-only command fields are omitted because execution is automatic (R3, R11).
- skill authority is expressed as local `project`/`global` scope (R15-R17).
- `skills.install` is a new contract based on installer semantics (R14, R18-R20).
- MCP structured results and truthful annotations are transport adaptations (R2-R3, R11).

The complete machine-readable delta registry is
`tests/conformance/fixtures/compatibility-deltas.json`.

