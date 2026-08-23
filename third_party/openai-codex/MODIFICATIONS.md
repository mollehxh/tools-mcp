# OpenAI Codex compatibility modifications

Pinned upstream commit: `8cabf5a6cf103cebe338d46346e43e3201e64f41`.

Files classified `unchanged` in `SOURCE.toml` are byte-identical. Local adapters live outside
the upstream subtrees. The frozen MCP fixtures intentionally adapt the pinned contracts as follows:

Adapter boundaries require source-level hashes. Boundaries created before that invariant carry an
explicit legacy exemption whose exact symbol and rationale are pinned in the offline verifier.

- `apply_patch` transports the original freeform patch through one MCP object field (R6).
- approval/escalation-only command fields are omitted because execution is automatic (R3, R11).
- skill authority is expressed as local `project`/`global` scope (R15-R17).
- skill catalog list/read, cursor paging, project precedence, and package-relative resources adapt
  the pinned extension tools to no-follow project/global host capabilities (R7-R9, R15-R17).
- MCP structured results and truthful annotations are transport adaptations (R2-R3, R11).
- unified exec is extracted behind an owner-scoped, transport-neutral registry; live slots are
  never evicted, transport handoff controls publication/consumption, and one five-minute terminal
  tombstone replaces the upstream session-owned store (R4-R5, R13).
- command spawn is routed through the shared verified workspace-write sandbox instead of Codex
  approval, hook, telemetry, and agent-session orchestration (R10-R12).
- apply-patch keeps the pinned streaming parser, fuzzy matcher, update algorithm, diagnostics,
  summaries, and partial-patch ordering, while an adapter replaces `ExecutorFileSystem` and
  `PathUri` with fixed-workspace preflight plus handle-relative, no-follow atomic mutations
  (R3, R6-R10).

The complete machine-readable delta registry is
`tests/conformance/fixtures/compatibility-deltas.json`.

## Built-in `skill-installer`

The retained package under `third_party/openai-codex/skill-installer/` comes from
`codex-rs/skills/src/assets/samples/skill-installer/` at the pinned commit. The two Python files,
package license, agent metadata, and both image assets are byte-identical to upstream;
`scripts/list-skills.py` is intentionally omitted.

Only `SKILL.md` is adapted. Its installer behavior and options remain unchanged, while unsupported
curated/experimental listing and sandbox-escalation instructions are removed. The replacement
instructions use the immutable `$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer` path bridge, retain
the script's default global `$CODEX_HOME/skills` destination, document project installation through
`--dest <workspace>/.agents/skills`, and require ordinary `exec_command` execution (R14-R18).
