# VPS authentication seam

The direct loopback workflow remains a development mode and must not be exposed
publicly. The production VPS gateway now owns durable self-hosted OAuth over the
stable ngrok HTTPS domain. It persists grants, keyed token hashes, refresh
families, revocation, owner consent state, device state, and monotonic public-ID
allocation in SQLite plus a rollback-excluded watermark.

Authentication belongs ahead of the transport-only MCP adapter. The U1 spike
demonstrates this boundary. The production gateway must authenticate each
request, derive a bounded
owner identity and deployment policy, and pass that owner context into the
existing application capabilities. HTTP admission, audit classification,
workspace selection, skill roots, and process ownership may become
deployment-specific.

The model-visible contract remains exactly these five tools:

- `exec_command`
- `write_stdin`
- `apply_patch`
- `skills.list`
- `skills.read`

Authentication tokens, login prompts, tenant IDs, and deployment state must
not become tool arguments or another tool. The future design must preserve
the existing schemas, result/error classifications, structured-content
fallback, and owner-scoped session semantics. Authorization headers and
credentials remain excluded from logs.

The gateway implements protected-resource and authorization-server discovery,
strict client/redirect/resource binding, PKCE S256, short-lived bearer tokens,
rotating refresh tokens, and family-wide replay revocation. Ordinary restart
preserves consent; matched rollback requires an unchanged security revision and
allocator; stale restore revokes every OAuth grant and worker identity and
requires fresh human authorization.

MCP protocol `2026-07-28` is stateless and does not provide an MCP session ID.
The live checkpoint established stable per-conversation OpenAI correlation
headers. Only the private trusted ingress supplies their salted fingerprints to
the gateway; raw values are never persisted or logged. Owner identity alone is
not used because it would merge concurrent conversations into one routing
fence.
