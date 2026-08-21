# Manual ChatGPT/ngrok release checklist (macOS)

This file is a manual release gate, not automated evidence. Do not mark an item
passed unless a human observed it in a disposable macOS profile. Preserve the
date, ChatGPT account/client configuration, mcp-agent archive checksum, ngrok
authority, and redacted result IDs with the release record.

## Isolation and inventory

- [ ] Create a disposable macOS user/profile with synthetic environment
  variables, credentials, local services, workspace, global skill root, and
  readable/writable outside sentinels.
- [ ] Record the initial project/global skill inventory and sentinel hashes.
- [ ] Verify the packaged archive and `SHA256SUMS`; start the package from a
  workspace path containing spaces.
- [ ] Confirm the startup warning names command execution, host reads, local
  service effects, workspace writes, and durable project/global installation.

## Loopback and tunnel

- [ ] Run `cargo run -p xtask -- inspector-smoke` and retain its output.
- [ ] Start with the exact ngrok host via `--public-host` and the exact client
  origin via `--origin`; start ngrok externally.
- [ ] Prove an unrelated `Host`, forwarded host, and unrelated `Origin` are
  rejected, while the configured HTTPS `/mcp` endpoint connects.
- [ ] In ChatGPT developer mode, rescan and confirm exactly the six frozen
  schemas. Record client-side denials separately from server failures.

## Prompts and expected observations

- [ ] Direct: ask for `printf chatgpt-mcp-ok`; verify output and exit code.
- [ ] Follow-up: start a yielded command, then ask ChatGPT to poll or write to
  the returned `session_id`; verify only incremental output is returned.
- [ ] Patch/write: ask for a synthetic workspace file edit; verify the file and
  confirm the outside writable sentinel is unchanged.
- [ ] Skills: install one synthetic public skill into `project` and one into
  `global`, list both, read both exact handles, and verify persistence after a
  server restart.
- [ ] Unsupported: request a seventh/nonexistent tool and an outside-workspace
  direct write; verify a clear denial and unchanged sentinel.

## Cleanup

- [ ] Inventory project/global skills, remove only test-installed packages,
  stop the tunnel, and send Ctrl-C to `mcp-agent`.
- [ ] Confirm descendant processes terminated, the endpoint closed, and no
  live/tombstone session remains reachable.
- [ ] Confirm outside sentinel hashes are unchanged; delete synthetic
  credentials/services/profile and rotate exposed ngrok credentials.
- [ ] Record final result: **not run / passed / failed / client-policy blocked**,
  with observed date and redacted notes. The repository copy defaults to **not
  run** and carries no claim that ChatGPT validation occurred.
