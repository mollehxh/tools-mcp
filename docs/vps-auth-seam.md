# Future VPS authentication seam

The current ngrok workflow is intentionally unauthenticated development mode.
It must not be promoted as a stable VPS deployment.

Authentication belongs ahead of the transport-only MCP adapter. A future
gateway or server middleware should authenticate the request, derive a bounded
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

OAuth discovery, token issuance/rotation, multi-user workspace provisioning,
durable deployment state, monitoring, and operational recovery are deferred.
