# VPS deployment and operations

This deployment shares one Ubuntu VPS with Amnezia VPN and the existing Yandex
Mail MCP. It must never claim host port 443 or start a second tunnel for the
stable ngrok hostname.

## Fixed topology

| Surface | Binding | Owner |
|---|---|---|
| Amnezia HTTPS/VPN | public `:443` | existing Docker/Amnezia deployment |
| SSH | public `:22` | host sshd |
| tools-mcp relay | public `:8444`, mandatory mTLS | systemd socket to gateway `127.0.0.1:8445` |
| tools-mcp OAuth/MCP | `127.0.0.1:8443`, TLS | `tools-mcp-gateway` |
| tenant `u1` relay | `127.0.0.1:8456`, mandatory mTLS | `tools-mcp-tenant-gateway@u1` |
| tenant `u1` OAuth/MCP | `127.0.0.1:8453`, TLS | `tools-mcp-tenant-gateway@u1` |
| shared path router | `127.0.0.1:8000`, HTTP | HAProxy |
| public HTTPS | ngrok cloud edge `:443` | one VPS ngrok agent to the router |
| Yandex Mail MCP | Docker network `:3000` | existing healthy container |

No workload port is published. There is no host listener on port 80 and no
tools-mcp listener on host port 443. The old `yandex-mail-ngrok-1` container is
intentionally stopped and must not be started implicitly.

The shared router accepts only these paths:

| Backend | Exact paths |
|---|---|
| Yandex Mail | `/yandex-mail/mcp`, `/yandex-mail/register`, `/yandex-mail/authorize`, `/yandex-mail/token`, `/yandex-mail/revoke`, `/yandex-mail/oauth/yandex/callback`, `/.well-known/oauth-protected-resource/yandex-mail/mcp`, `/.well-known/oauth-authorization-server/yandex-mail` |
| tools-mcp | `/mcp`, `/authorize`, `/token`, `/.well-known/oauth-protected-resource/mcp`, `/.well-known/oauth-authorization-server`, `/.well-known/openid-configuration`, `/oauth-client/chatgpt.json` |
| tenant `u1` | `/u1/mcp`, `/u1/authorize`, `/u1/token`, `/u1/.well-known/oauth-protected-resource/mcp`, `/u1/.well-known/oauth-authorization-server`, `/u1/.well-known/openid-configuration`, `/u1/oauth-client/chatgpt.json`, `/.well-known/oauth-protected-resource/u1/mcp`, `/.well-known/oauth-authorization-server/u1` |

Every other path returns 404. Route matching is case-sensitive and exact.
HAProxy obtains the healthy Yandex container's private IP during startup and
validates the tools-mcp upstream certificate against the private CA. The
stable hostname must have only one owning ngrok endpoint.

## Build and verify the VPS release

On a native Linux builder, run:

```sh
cargo run -p xtask -- vps-package
```

This produces `tools-mcp-vps-<version>-<linux-target>.tar.gz` and its detached
SHA-256 file under `target/release-artifacts/`. The bundle contains only the
Linux gateway, admin utility, VPS runner, deployment scripts/configuration,
container assets, operations guide, license, manifest, and checksums. It is not
a general Linux local-worker package. The package verifier rejects missing,
extra, non-regular, traversal-named, mode-mismatched, or checksum-mismatched
files. CI assembles the same inputs twice and requires byte-identical archives.

Verify the detached digest and the extracted manifest before staging a new
versioned release under `/opt/tools-mcp/releases`; never deploy loose binaries
from `target/release`.

## Accounts and persistent state

- `tools-mcp-gateway`: gateway process and `/var/lib/tools-mcp/gateway`.
- `tools-mcp-edge`: ngrok process; it can read only its root-managed config.
- `tools-mcp-workload`: rootless Podman graph, runner, workspace, and home.
- `/var/lib/tools-mcp/workload`: 36 GiB ext4 graph/root-layer volume.
- `/var/lib/tools-mcp/workspace`: 20 GiB ext4 volume mounted at container
  `/workspace`.
- `/var/lib/tools-mcp/home`: 4 GiB ext4 volume mounted at container
  `/home/developer`.

The combined workload cap is 60 GiB and leaves at least 15 GiB for the host,
Docker/Amnezia, SSH, gateway, WAL, logs, backups, and one old plus one new
image. Byte and inode admission are checked independently. Rootless Podman is
fixed to 1.5 CPU, 2.5 GiB memory, 512 PIDs, and 4096 descriptors.
The container supervisor keeps `PrivateDevices=yes` and bind-mounts only
`/dev/net/tun` for slirp4netns, with a matching systemd device allow rule; no
disk, Docker/Podman socket, or other host device is exposed.
Do not add systemd `ProtectKernelTunables`, `ProtectKernelModules`, or
`ProtectKernelLogs` to this supervisor: their locked `/proc` submounts enter
Podman's persistent rootless pause namespace and prevent runc from mounting the
container's private procfs. The supervisor is already an unprivileged user
without host capabilities; `rootless-preflight` additionally rejects block
devices and `/dev/kmsg` inside that pause namespace.
For the same reason, the supervisor does not use systemd `PrivateTmp`: the
slirp4netns sandbox must mount its own tmpfs over `/tmp`. The workload itself
still receives the container's private filesystem and namespaces; preflight
proves the required nested mount before every start.

## Baseline and metrics activation

Before the first mutation, run `capture-baseline` into a new root-only
directory. After reviewing it, point `/var/lib/tools-mcp/baseline/current` at
that immutable directory. The baseline contains stable Docker-daemon and
Amnezia configuration fingerprints in addition to the full diagnostic
capture; do not regenerate it to make drift disappear.

Install `metrics.env`, `tools-mcp-metrics.service`, and
`tools-mcp-metrics.timer`, enable the timer, and run the service once before
public activation. The root collector atomically writes only allowlisted,
unlabelled numeric host metrics to
`/var/lib/tools-mcp/metrics/host.prom`. Gateway events are one-line JSON with
opaque owner/workspace/lease/call IDs plus fixed backend, generation, tool,
duration, disposition and error class. Commands, tool arguments and output,
patches, skill contents, absolute paths, headers, cookies and credentials are
never event or metric fields.

For a privacy drill, create a root-only canary file containing one unique value
for each seeded token, header, command argument, output, absolute path, patch
and skill body, then run `privacy-scan CANARY_FILE`. It scans project journald,
the metrics snapshot, gateway state, every recovery set, and rejects any
project core-dump record. The canaries must be synthetic: do not duplicate the
real owner secret, CA key, token-hash key or relay private key that a matched
root-only recovery set intentionally contains. All project services and the
workload container have core dumps disabled.

The combined endpoint is host-only and is deliberately absent from HAProxy's
public allowlist:

`sudo curl --fail --silent --show-error --cacert /etc/tools-mcp/pki/gateway/device-ca.pem https://127.0.0.1:8443/metrics`

It reports backend eligibility/compatibility/generation, local lease age,
in-flight calls, relay connections/rejections/drops, refresh outcomes, call and
HTTP histograms plus p95, process start, SQLite integrity/schema, backup and
certificate age, workload/host bytes and inodes, cgroup OOM/PID/CPU pressure,
journal growth, systemd restarts, route probes, and Docker/Amnezia baseline
matches. A missing or malformed root snapshot returns 503 instead of partial
host data.
The collector is ordered after the rootless container so its read-only Podman
inspection joins the already verified pause namespace; it must never create
that namespace ahead of the container supervisor. Gateway startup is not
coupled to metrics startup: a missing snapshot makes only `/metrics` return
503, while protected MCP readiness still depends on the fallback runner.

SSH and VPN must be tested from a separate client; a self-probe cannot prove
their external data planes. Immediately after each successful external test,
record the result from a human SSH session:

```sh
sudo /opt/tools-mcp/current/libexec/record-external-probe ssh 1
sudo /opt/tools-mcp/current/libexec/record-external-probe vpn 1
sudo systemctl start tools-mcp-metrics.service
```

The records expire after ten minutes by default. Never record a success that
was not just observed externally.

## Checkpointed upgrade

1. Record v4/v6 listeners, routes/firewall, Docker configuration and container
   IDs, Amnezia inspection, Yandex health/routes, external SSH/VPN behavior,
   filesystems/inodes, user namespaces, and free-space projection. Stop if
   443 changed owner, 80/8444 is unexpectedly occupied, Yandex/VPN/SSH is
   unhealthy, Docker needs a restart, or the reserve cannot hold old and new
   artifacts.
2. Build and verify the immutable `vps-package` artifact and the digest-pinned
   workload image inactive.
   Keep the prior release, image, unit files, configs, and container through
   the seven-day soak.
3. Stop runner, then gateway. Run `backup-state`; only a directory containing
   a verified `SHA256SUMS` is a recovery set. Atomically switch
   `/opt/tools-mcp/current`.
4. Start gateway, verified container, then runner. A private health response is
   503 until a compatible fresh VPS generation registers.
5. Start the loopback shared router, public relay socket, and the single ngrok
   agent. Compare every Yandex/tools/unmatched path externally, then SSH, VPN,
   Docker IDs, Amnezia, storage, and p95 latency.

Rollback stops only the new ngrok/relay surfaces, restores the previous router,
units, config, release symlink and container, and retains `/workspace`,
`/home`, rootless graph storage, and rejected artifacts. Never delete or
restart Docker/Amnezia as a rollback shortcut. Restore the database only if no
later authorization or allocator write committed.

## Recovery classes

- **Ordinary reboot:** mount bounded storage, validate SQLite/secrets, start
  gateway, verify/start rootless container, register a fresh runner generation,
  then start router/ngrok/relay. OAuth and GitHub login remain valid; volatile
  terminals are lost.
- **Matched deploy rollback:** writers were quiesced and the current security
  revision plus allocator watermark still equal the recovery set. Restore the
  matched set and start services explicitly.
- **Stale/disaster restore:** preserve the rollback-excluded allocator
  watermark, restore the data, revoke all OAuth grants/token families and
  worker identities, and require new consent and enrollment. Never reuse an
  old public ID.

Run `restore-state matched RECOVERY_SET` or `restore-state stale RECOVERY_SET`
only with gateway and runner stopped. A checksum, SQLite integrity, key-ID,
revision, or watermark mismatch fails closed. Same-disk recovery sets and Git
do not protect against total VPS disk loss.

If a reboot leaves port 22 accepting TCP without an SSH banner, use the VPS
provider console. Inspect `systemctl --failed`, `journalctl -b`, `findmnt`, and
the three marked tools-mcp entries in `/etc/fstab`; restore the retained
pre-storage `fstab.before` only if a tools-mcp mount prevents normal boot. Do
not alter Docker, Amnezia, or firewall state while diagnosing storage.

## Human-only GitHub setup

From an SSH-controlled session, enter the container and run `gh auth login`.
Use a dedicated identity when practical and the narrowest fine-grained
repository scopes. Test private clone, fetch, commit, and push, then review
scopes periodically. Arbitrary container-root commands can read and transmit
this credential; it is persistent authority, not a secret from the model.

## Isolated tenant MVP

The first additional-user deployment is an intentionally narrow per-tenant
stack, not a shared multi-tenant gateway. Tenant `u1` uses the public MCP URL
`https://fidela-unsubversive-imaginarily.ngrok-free.dev/u1/mcp`. HAProxy
admits only the enumerated `/u1` MCP/OAuth discovery paths, strips the prefix
for the private gateway, and leaves the owner and `/yandex-mail/*` routes
unchanged. Public 443 remains owned by the existing Amnezia/Docker stack.

Each tenant instance has its own `tools-mcp-<tenant>` Linux identity, rootless
Podman graph, container, gateway/runner processes, OAuth database and keys,
owner secret, `/workspace`, and `/home/developer`. Tenant directories are mode
0700. A tenant cannot read the owner's home, and the owner workload identity
cannot read a tenant home. `gh auth login` must be performed separately inside
each tenant container; credentials are never copied from the owner container.
The tenant egress policy does not include the owner's SSH-to-host exception.

Provisioning is explicit and currently operator-driven:

```sh
sudo /opt/tools-mcp/current/libexec/provision-tenant-mvp \
  --apply u1 8453 8456 /path/to/workspace.oci sha256:<image-digest>
sudo systemctl enable --now tools-mcp-tenant-gateway@u1.service
sudo systemctl enable --now tools-mcp-tenant-container@u1.service
sudo systemctl enable --now tools-mcp-tenant-runner@u1.service
```

Retrieve a tenant's bootstrap secret only over SSH; do not copy it into logs or
shared documentation:

```sh
sudo cat /root/.tools-mcp-u1-owner-secret
```

This MVP provides VPS execution only. Tenant local-agent enrollment,
automatic tenant creation/removal, quotas beyond the container cgroup limits,
and an operator UI are deferred. Adding another tenant requires a new exact
route set and distinct private ports; never point two users at one tenant.

Tenant gateway and runner certificates are currently issued for 90 days.
Automatic renewal is not part of this MVP: schedule an operator rotation well
before day 75, replace both certificate/key pairs atomically, and restart the
tenant gateway and runner only after validating the new chain. The seven-day
certificate gate remains the final fail-closed protection, not the renewal
schedule.

## Human-only enrollment and revocation

Local enrollment starts with `tools-mcp enroll-device` on macOS or Windows.
Over SSH, compare the locally displayed DER CSR fingerprint with a fresh
`openssl req -verify`/SHA-256 calculation before approving the exact device,
platform and expiry with `tools-mcp-admin approve-device-csr`. Install the
returned chain locally; ordinary daily startup then needs no SSH interaction.

To revoke a device or ChatGPT grant, first identify the exact opaque ID in the
root-only SQLite state, then run `tools-mcp-admin revoke-device DEVICE_ID` or
`tools-mcp-admin revoke-grant GRANT_ID` with `/etc/tools-mcp/gateway.env`
loaded. Device revocation closes the relay; grant revocation rejects its
tokens, cancels its calls and invalidates its handles. Emergency stale restore
uses `stale-restore-revoke-all`. Consent, enrollment, revocation, GitHub login
and host administration always require a browser, local console or SSH; none
is exposed as a model tool.

## Operator gates

At +5 minutes, +1 hour, +24 hours, and +7 days record:

- external tools and Yandex metadata/MCP probes plus one harmless five-tool
  smoke;
- OAuth refresh, active/revoked grants, eligible VPS generation, lease age,
  in-flight count, relay saturation/drops, and reconnect latency;
- gateway restarts/5xx/p95, SQLite integrity/schema, recovery-set age and test
  restore, certificate lifetime, log growth, bytes/inodes, and cgroup
  OOM/PID/CPU pressure;
- external SSH and VPN data plane, port 443 owner, Docker IDs/config, Amnezia
  state, and the intentionally stopped legacy ngrok container.

At each checkpoint, retain the UTC time, release/config/image digests, the
complete numeric `/metrics` snapshot, route status codes, external SSH/VPN
evidence, and the five-tool smoke result under the root-only deployment
checkpoint. Compute a SHA-256 manifest after the record is closed. Derive p95
from `tools_mcp_gateway_http_p95_ms` and the idle relay conformance run over at
least 100 samples; tool-duration p95 includes actual command time and is not a
relay-overhead measurement.

After the external probes and a ChatGPT five-tool smoke have just passed,
write `result=pass` into a root-only evidence file and atomically record the
checkpoint. The recorder rejects missing or expired SSH/VPN evidence, an
ineligible VPS, routing or Docker/Amnezia drift, queue drops, p95 above 100 ms,
and certificates inside the seven-day deployment no-go window:

```sh
sudo /opt/tools-mcp/current/libexec/record-operator-checkpoint 5m /root/five-tool-smoke.txt
```

Use `1h`, `24h`, and `7d` for the later immutable checkpoints. Existing
checkpoint names cannot be overwritten.

Roll back on any containment loss, rootful Podman fallback, queue drop,
credential resurrection or leak, SQLite/key mismatch, Docker/Amnezia drift,
VPS readiness beyond five seconds, two consecutive external SSH/VPN failures,
or gateway/relay p95 above 100 ms for the recorded sample window. The +24-hour
checkpoint retains rollback artifacts; the +7-day soak is the release gate.
