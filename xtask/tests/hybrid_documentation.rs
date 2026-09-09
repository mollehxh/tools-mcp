use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read(relative: &str) -> String {
    fs::read_to_string(root().join(relative)).unwrap()
}

#[test]
fn operator_docs_preserve_routes_platform_evidence_and_execution_boundaries() {
    let installation = read("docs/installation.md");
    let routing = read("docs/hybrid-routing.md");
    let windows = read("docs/windows-installation.md");
    let deployment = read("docs/vps-deployment.md");
    let security = read("docs/security-model.md");
    let release_notes = read("docs/release-notes/0.1.0-hybrid.md");
    let owner_mode = read("docs/owner-mode.md");

    for tool in [
        "`exec_command`",
        "`write_stdin`",
        "`apply_patch`",
        "`skills.list`",
        "`skills.read`",
    ] {
        assert!(installation.contains(tool));
        assert!(routing.contains(tool));
    }
    for statement in [
        "never synchronize automatically",
        "never replayed",
        "`backend_changed`",
        "`/workspace`",
    ] {
        assert!(
            routing.contains(statement),
            "missing routing statement: {statement}"
        );
    }
    assert!(installation.contains("selects the nearest Git ancestor inside that MCP session"));
    assert!(installation.contains("never recursively combines sibling repositories"));
    assert!(routing.contains("Project-skill selection is also session-scoped"));
    assert!(security.contains("rooted through\nno-follow directory capabilities"));
    assert!(installation.contains("npm install --global tools-mcp"));
    assert!(installation.contains("cd ~/dev/some-project\ntools-mcp"));
    assert!(installation.contains("private device key remains\nnon-exportable"));
    assert!(windows.contains("npm install --global tools-mcp"));
    assert!(windows.contains("Set-Location C:\\dev\\some-project\ntools-mcp"));
    assert!(windows.contains("automatically use the VPS workspace"));
    assert!(windows.contains("has **not** been performed"));
    assert!(windows.contains("Linux local-agent packaging is also deferred"));
    assert!(security.contains("credential is deliberately readable"));
    assert!(deployment.contains("must never claim host port 443"));
    assert!(deployment.contains("must not be started implicitly"));
    assert!(deployment.contains(
        "`sudo curl --fail --silent --show-error --cacert /etc/tools-mcp/pki/gateway/device-ca.pem https://127.0.0.1:8443/metrics`"
    ));
    assert!(deployment.contains("record-external-probe ssh 1"));
    assert!(deployment.contains("record-external-probe vpn 1"));
    assert!(deployment.contains("cargo run -p xtask -- vps-package"));
    assert!(deployment.contains("bind-mounts only\n`/dev/net/tun`"));
    assert!(deployment.contains("prevent runc from mounting"));
    assert!(release_notes.contains("No live\n  Windows-to-ChatGPT walkthrough is claimed"));
    assert!(release_notes.contains("sole owner of\n  host port 443"));
    assert!(release_notes.contains("Seven days is the final release gate"));
    assert!(owner_mode.contains("permits TCP port 22 to that exact address"));
    assert!(owner_mode.contains("Do not connect other users to this container"));
    assert!(owner_mode.contains("or implement multi-user isolation"));
}

#[test]
fn shared_router_has_exact_allowlists_and_never_exposes_operations_paths() {
    let config = read("deploy/vps/haproxy/shared-router.cfg.in");
    let yandex = [
        "/yandex-mail/mcp",
        "/yandex-mail/register",
        "/yandex-mail/authorize",
        "/yandex-mail/token",
        "/yandex-mail/revoke",
        "/yandex-mail/oauth/yandex/callback",
        "/.well-known/oauth-protected-resource/yandex-mail/mcp",
        "/.well-known/oauth-authorization-server/yandex-mail",
    ];
    let tools = [
        "/mcp",
        "/authorize",
        "/token",
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration",
        "/oauth-client/chatgpt.json",
    ];
    let tenant_u1 = [
        "/u1/mcp",
        "/u1/authorize",
        "/u1/token",
        "/u1/.well-known/oauth-protected-resource/mcp",
        "/u1/.well-known/oauth-authorization-server",
        "/u1/.well-known/openid-configuration",
        "/u1/oauth-client/chatgpt.json",
        "/.well-known/oauth-protected-resource/u1/mcp",
        "/.well-known/oauth-authorization-server/u1",
    ];
    for path in yandex {
        assert!(config.contains(&format!("acl yandex_path path {path}\n")));
    }
    for path in tools {
        assert!(config.contains(&format!("acl tools_path path {path}\n")));
    }
    assert!(config.contains(&format!(
        "acl tenant_u1_path path {}\n",
        tenant_u1.join(" ")
    )));
    assert!(config.contains("server tenant_u1_gateway 127.0.0.1:8453 ssl"));
    assert!(config.contains("http-request set-path %[path,regsub(^/u1,)]"));
    assert!(!config.contains("acl tenant_u1_path path_beg"));
    assert!(!config.contains("path /metrics\n"));
    assert!(!config.contains("path /healthz\n"));
    assert!(config.contains("default_backend unmatched"));
    assert!(config.contains("http-request return status 404"));
}

#[test]
fn deployment_smoke_submits_the_current_consent_contract() {
    let smoke = read("deploy/vps/scripts/oauth-mcp-smoke");

    assert!(smoke.contains("\"decision\": \"approve\""));
    assert!(smoke.contains("context.load_verify_locations(cafile=ca_file)"));
    assert!(!smoke.contains("ssl.CERT_NONE"));
    assert!(!smoke.contains("check_hostname = False"));
}

#[test]
fn tenant_mvp_keeps_identity_storage_processes_and_credentials_separate() {
    let provision = read("deploy/vps/scripts/provision-tenant-mvp");
    let gateway = read("deploy/vps/systemd/tools-mcp-tenant-gateway@.service");
    let container = read("deploy/vps/systemd/tools-mcp-tenant-container@.service");
    let supervisor = read("deploy/vps/scripts/supervise-container");
    let runner = read("deploy/vps/systemd/tools-mcp-tenant-runner@.service");

    assert!(provision.contains("account=tools-mcp-$tenant"));
    assert!(provision.contains("base=/var/lib/tools-mcp-tenants/$tenant"));
    assert!(provision.contains("TOOLS_MCP_CLIENT_ID=https://chatgpt.com/oauth/client.json"));
    assert!(provision.contains("TOOLS_MCP_ALLOW_OWNER_SSH=0"));
    assert!(provision.contains("$base/workspace"));
    assert!(provision.contains("$base/home"));
    assert!(provision.contains("$base/gateway/gateway.sqlite3"));
    assert!(gateway.contains("User=tools-mcp-%i"));
    assert!(container.contains("tools-mcp-%i-workspace"));
    assert!(container.contains("Type=notify"));
    assert!(container.contains("Restart=on-failure"));
    assert!(supervisor.contains("systemd-notify --ready"));
    assert!(supervisor.contains("podman wait \"$TOOLS_MCP_CONTAINER\""));
    assert!(runner.contains("/etc/tools-mcp/tenants/%i/runner.env"));
    assert!(!provision.contains("/var/lib/tools-mcp/workspace"));
    assert!(!provision.contains("/var/lib/tools-mcp/home"));
}

#[test]
fn npm_package_smoke_is_registry_and_user_cache_independent() {
    let smoke = read("npm/tools-mcp/test/package-smoke.js");

    assert!(smoke.contains("\"--offline\""));
    assert!(smoke.contains("npm_config_cache: npmCache"));
}
