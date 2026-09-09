# Owner SSH mode

The current release has one owner and one VPS workspace. An administrator can
opt this workspace into host SSH access by installing a root-owned, mode 0644
`/etc/tools-mcp/owner-ssh-address` containing the host's public IPv4 address.
`configure-egress` then permits TCP port 22 to that exact address. SSH still
requires normal authentication; no password or private key is provisioned by
this setting. Other host ports and private network restrictions remain in place.

This is an owner workspace capability, not per-request multi-user authorization.
Do not connect other users to this container. A future multi-user deployment
must assign separate containers, homes, credentials and routing to each user;
only the owner's container may receive this setting. Root SSH access can access
all host data, including other users' data. Ordinary tenants must never inherit
the owner's SSH keys or configuration.

## Live installation, 2026-09-07

The approved single-owner exception is installed separately from the existing
release at `/opt/tools-mcp/owner-ssh-v1`. Container and runner systemd drop-ins
apply it after network preparation. The runner environment was checked through
`/proc` after restart and points to the owner wrapper; gateway and runner are
active. A connection from the running container reaches SSH host-key validation
on the VPS. Authentication has not been tested and no credentials were installed.
The host key must be verified before the owner's first authenticated connection.

The live owner container has a dedicated non-password SSH key stored in the
persistent `/home/developer/.ssh` directory. Its public key is authorized only
for the VPS root account, the verified host key is pinned in `known_hosts`, and
the alias `ssh vps` uses that identity. The container image links root's OpenSSH
configuration to the persistent home because OpenSSH resolves root through
`/etc/passwd` rather than the `HOME` environment variable.

This live change only enables host SSH networking. It does not remove the
existing workdir restriction or implement multi-user isolation.
