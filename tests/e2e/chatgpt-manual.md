# Manual ChatGPT HTTPS/OAuth checkpoint (U1)

This was the blocking U1 release gate and passed live on 2026-08-24. It is
evidence collection, not an automated test. Never record
the owner secret, authorization codes, access tokens, refresh tokens, cookies,
or complete request metadata.

## Immutable baseline

- [x] Confirm the account-owned stable ngrok domain resolves and routes to the
  supervised tunnel on this VPS.
- [ ] On the VPS, record IPv4 and IPv6 listeners and the Docker/Amnezia
  container, network, route, and firewall baseline.
- [ ] Prove external SSH and VPN data-plane traffic work before any change.
- [x] Stop if TCP 8443 is already owned, the baseline is unhealthy, or the
  tunnel would require changing/restarting Docker, Amnezia, the host firewall,
  or the listener on 443.
- [ ] Keep a recoverable SSH session open. Record the exact pre-test owner of
  TCP 443 and its container/image/config identity.

## Start the U1 endpoint

- [x] Start the stable free ngrok edge and forward it only to the spike's TLS
  listener without changing VPS TCP 443.
- [ ] Export `U1_OWNER_SECRET` from a protected interactive prompt or root-only
  environment file; do not place it on the command line or in shell history.
- [ ] Set `U1_ALLOWED_CLIENT_ID` to the exact HTTPS ChatGPT CIMD client ID and
  `U1_ALLOWED_REDIRECT_URI` to the exact redirect URI observed during setup.
- [ ] Start:

  ```sh
  cargo run -p xtask -- transport-spike-oauth-serve \
    127.0.0.1:8443 https://fidela-unsubversive-imaginarily.ngrok-free.dev CERT_PEM KEY_PEM
  ```

- [x] Externally verify TLS, OAuth protected
  resource discovery, authorization-server discovery, and a 401 bearer
  challenge on `https://fidela-unsubversive-imaginarily.ngrok-free.dev/mcp`.
- [ ] Confirm the endpoint never serves plaintext and TCP 443 is byte-for-byte
  unchanged from the recorded baseline.

## ChatGPT observation

- [ ] Add the MCP endpoint in ChatGPT developer mode and complete owner consent
  once. Record whether client registration used CIMD or DCR and the exact
  redirect URI, with credentials redacted.
- [x] Record every MCP protocol version ChatGPT actually sends. The checkpoint
  requires `2026-07-28` and exactly the five frozen tools:
  `exec_command`, `write_stdin`, `apply_patch`, `skills.list`, `skills.read`.
- [x] Wait beyond the 30-second access-token lifetime and perform harmless
  discovery/calls long enough to observe at least two successful refresh-token
  exchanges. The consent page must not return. Record only the exchange count.
- [x] Start two distinct ChatGPT conversations and inspect an allowlist of
  trusted ingress headers. Prove one stable, conversation-specific key
  is present across multiple calls in one conversation and differs in the
  other. Record the field name as `stable:<field>`; do not record its value.
- [ ] If there is no such field, record `unknown`. That is a failed U1 gate:
  modern stateless MCP has no session header that can safely substitute for a
  conversation identity, so U2 must not begin.

## Negative and coexistence checks

- [ ] Verify wrong client ID, redirect URI, resource, owner secret, missing
  `offline_access`, and non-S256 PKCE are denied.
- [ ] Verify an authorization code cannot be replayed and a replayed refresh
  token revokes its current token family.
- [ ] Repeat external SSH and VPN data-plane probes while ChatGPT performs the
  five-tool smoke test. Re-record listeners, Docker/Amnezia identity, routes,
  and firewall state; any unexpected diff fails the checkpoint.

## Record and clean up

- [ ] Stop the spike, confirm 8443 is closed, and remove only U1 material.
  Preserve the trusted certificate only if its storage and renewal policy are
  already safe. Do not touch 443, Docker, or Amnezia.
- [ ] Fill `tests/e2e/chatgpt-scan-tools-checkpoint.toml`: exact endpoint,
  registration mechanism, protocol versions, redirect URIs, refresh count,
  `context_correlation`, and `port_443_unchanged = true`.
- [ ] Set `status = "passed"` only if all checks succeeded; retain date,
  redacted notes, command output, config/artifact digests, and before/after
  evidence outside git. Then run
  `cargo run -p xtask -- transport-spike-verify-chatgpt`.

The repository checkpoint records only redacted, privacy-safe evidence. Raw
OpenAI session/subject headers and OAuth credentials are not retained.
