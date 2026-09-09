# Windows local worker

Install the same user-facing npm command as on macOS. npm selects the native
Windows package automatically; Node only launches the signed-in-place native
worker and never executes MCP tools itself:

```powershell
npm install --global tools-mcp
```

After the one-time human-controlled enrollment, store the public relay paths
once with `tools-mcp setup`. Every normal launch is then just the project
directory followed by the bare command:

```powershell
Set-Location C:\dev\some-project
tools-mcp
```

The launch directory is fixed as the workspace for that process. Starting the
command in another directory replaces the previous local worker and makes the
new directory current. If the command is not running or loses its verified
relay connection, new tool calls automatically use the VPS workspace.

The first release builds a native Windows archive in the `windows-latest` CI
job. Keep the extracted archive intact: `mcp-agent.exe`, the verified native
sandbox helper, compatibility manifest, canary, policy assets, checksums,
system skills, licenses, and notices are one package.

The archive is the low-level distribution contained by the platform-specific
npm package. For a manual or diagnostic install, verify the archive checksum
and its internal `SHA256SUMS`, then open PowerShell in the project that should
become current and run the packaged executable. PowerShell is the default
shell; explicit supported shell selection and PTY continuation use the same
five MCP tool schemas as macOS and the VPS.

Enrollment generates a non-exportable P-256 key in Windows CNG and emits a CSR.
An administrator must approve the displayed fingerprint over SSH before the
relay certificate is installed. A missing or altered helper, wrong target or
protocol, writable release boundary, certificate substitution, unavailable
restricted token, or unavailable Job Object prevents registration.

The worker uses a restricted token, a medium-integrity ceiling, declared-root
ACLs, a strict inherited-stdio handle list, suspended child assignment, and a
non-breakaway kill-on-close Job Object. Native CI exercises PowerShell,
PTY/input/Ctrl-C, workspace and escape probes, registry and inherited-handle
denial, CNG isolation, descendant cleanup, relay replacement, conformance, and
packaging from a path containing spaces and non-ASCII text.

Manual Windows-to-ChatGPT testing has **not** been performed because a Windows
workstation is currently unavailable. Do not describe CI evidence as a live
Windows walkthrough. Linux local-agent packaging is also deferred; Linux is
supported here only for the gateway and VPS runner.
