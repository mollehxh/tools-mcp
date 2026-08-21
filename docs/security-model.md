# macOS security model

The shipped backend is macOS Seatbelt. Linux bubblewrap and Windows restricted
token/helper backends are deferred and unsupported by this release.

At startup, `mcp-agent` canonicalizes the launch directory as an immutable
workspace, verifies the package compatibility set, and runs a native self-test.
Commands execute automatically under a deny-default Seatbelt policy that:

- permits host reads, process execution, outbound/local networking, and normal
  macOS services available to the account;
- permits direct filesystem writes only under the fixed workspace;
- protects workspace `.git`, `.codex`, `.mcp-agent`, and server staging paths;
- permits ordinary workspace editing of project `.agents/skills`; and
- keeps global skill installation behind server-owned, no-follow operations.

Policy, marker, protocol, target, and executable checksums are bound by the
release manifests. Assets are resolved relative to the executable; the native
launcher is the absolute system path `/usr/bin/sandbox-exec`. Package and
sandbox files are revalidated before execution so a writable install directory
does not silently turn replacement into an unsandboxed fallback.

This boundary contains direct command filesystem writes. It does not prevent
effects brokered by allowed services such as Docker, databases, local agents,
network APIs, or credential-bearing command-line tools. Readable secrets can
also be exfiltrated over allowed network access. The local tunnel is not
multi-user isolation, authentication, denial-of-service protection, or a VPS
security boundary.

Commands are automatic; there is no per-command approval escalation argument.
Startup fails closed if the package, native launcher, policy, self-test, or
workspace authority cannot be established.
