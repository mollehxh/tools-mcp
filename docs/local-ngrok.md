# Local ngrok development tunnel

An ngrok URL is an unauthenticated remote command endpoint. Anyone who obtains
it can execute commands, read files available to your macOS account, affect
local services, write every declared workspace/temp/cache/tool-state root,
bind workload listeners on any interface, and durably modify project/global
skills plus Cargo or Gradle state. Remote skills are unreviewed instructions or
executable content with the same authority when used. Use synthetic credentials
and a disposable macOS profile. Do not expose a personal home directory or a
production workstation.

Start `mcp-agent` with the exact ngrok authority. The value is a host (and an
explicit non-default port, if any), without `https://` or `/mcp`:

```sh
./mcp-agent --public-host example-name.ngrok.app --origin https://chatgpt.com
```

In another terminal, start the external tunnel:

```sh
ngrok http http://127.0.0.1:8000
```

Configure ChatGPT developer mode with:

```text
https://example-name.ngrok.app/mcp
```

The public `Host` must exactly match `--public-host`. Forwarded-host headers do
not grant access. Every request that carries an `Origin` must exactly match a
separately supplied `--origin`; do not use wildcards. If the client uses a
different documented origin, restart with that exact origin rather than
loosening validation. Requests with unrelated `Host` or `Origin` values are
rejected before tool work starts.

The warning printed by `mcp-agent` remains applicable for the whole tunnel
lifetime. A reserved or stable ngrok domain remains dangerous after a test:
possession of the live URL is sufficient authority even though it is not an
authentication credential.

Stop ngrok and `mcp-agent`, remove test-installed skills, delete synthetic
credentials, and rotate any tunnel credential that was exposed.

This is development mode only. See `docs/vps-auth-seam.md` for the future
authenticated deployment boundary.
