"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const test = require("node:test");

const cli = path.join(__dirname, "..", "bin", "tools-mcp.js");

test("setup persists credentials once and bare startup preserves cwd", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tools-mcp-npm-"));
  const config = path.join(root, "config", "config.json");
  const relayCa = path.join(root, "relay-ca.pem");
  const deviceCert = path.join(root, "device.pem");
  const project = path.join(root, "project");
  const capture = path.join(root, "capture.json");
  const native = path.join(root, "mcp-agent");
  fs.writeFileSync(relayCa, "ca");
  fs.writeFileSync(deviceCert, "cert");
  fs.mkdirSync(project);
  fs.writeFileSync(
    native,
    `#!/usr/bin/env node\nrequire('node:fs').writeFileSync(process.env.CAPTURE, JSON.stringify({cwd:process.cwd(),args:process.argv.slice(2)}));\n`,
    { mode: 0o755 },
  );
  const environment = {
    ...process.env,
    TOOLS_MCP_CONFIG_FILE: config,
    TOOLS_MCP_NATIVE_BINARY: native,
    CAPTURE: capture,
  };

  const setup = spawnSync(process.execPath, [
    cli,
    "setup",
    "--relay-url", "wss://relay.example:8444/relay",
    "--relay-ca", relayCa,
    "--device-cert", deviceCert,
    "--device-id", "mac-main",
  ], { env: environment, encoding: "utf8" });
  assert.equal(setup.status, 0, setup.stderr);
  assert.equal(fs.statSync(config).mode & 0o777, 0o600);

  const run = spawnSync(process.execPath, [cli], {
    cwd: project,
    env: environment,
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr);
  const observed = JSON.parse(fs.readFileSync(capture, "utf8"));
  assert.equal(observed.cwd, fs.realpathSync(project));
  assert.deepEqual(observed.args, [
    "--relay-url", "wss://relay.example:8444/relay",
    "--relay-ca", fs.realpathSync(relayCa),
    "--device-cert", fs.realpathSync(deviceCert),
    "--device-keychain-label", "tools-mcp-device:mac-main",
    "--device-id", "mac-main",
  ]);

  fs.rmSync(root, { recursive: true, force: true });
});

test("bare startup fails with setup guidance when config is absent", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tools-mcp-npm-"));
  const result = spawnSync(process.execPath, [cli], {
    env: { ...process.env, TOOLS_MCP_CONFIG_FILE: path.join(root, "missing.json") },
    encoding: "utf8",
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /run tools-mcp setup first/);
  fs.rmSync(root, { recursive: true, force: true });
});

test("setup can use a protected PEM key without Keychain prompts", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tools-mcp-npm-pem-"));
  const config = path.join(root, "config", "config.json");
  const relayCa = path.join(root, "relay-ca.pem");
  const deviceCert = path.join(root, "device.pem");
  const deviceKey = path.join(root, "device.key");
  const capture = path.join(root, "capture.json");
  const native = path.join(root, "mcp-agent");
  fs.writeFileSync(relayCa, "ca");
  fs.writeFileSync(deviceCert, "cert");
  fs.writeFileSync(deviceKey, "key", { mode: 0o600 });
  fs.writeFileSync(
    native,
    `#!/usr/bin/env node\nrequire('node:fs').writeFileSync(process.env.CAPTURE, JSON.stringify(process.argv.slice(2)));\n`,
    { mode: 0o755 },
  );
  const environment = {
    ...process.env,
    TOOLS_MCP_CONFIG_FILE: config,
    TOOLS_MCP_NATIVE_BINARY: native,
    CAPTURE: capture,
  };

  const setup = spawnSync(process.execPath, [
    cli,
    "setup",
    "--relay-url", "wss://relay.example:8444/relay",
    "--relay-ca", relayCa,
    "--device-cert", deviceCert,
    "--device-key", deviceKey,
    "--device-id", "mac-main",
  ], { env: environment, encoding: "utf8" });
  assert.equal(setup.status, 0, setup.stderr);

  const run = spawnSync(process.execPath, [cli], {
    env: environment,
    encoding: "utf8",
  });
  assert.equal(run.status, 0, run.stderr);
  assert.deepEqual(JSON.parse(fs.readFileSync(capture, "utf8")), [
    "--relay-url", "wss://relay.example:8444/relay",
    "--relay-ca", fs.realpathSync(relayCa),
    "--device-cert", fs.realpathSync(deviceCert),
    "--device-key", fs.realpathSync(deviceKey),
    "--device-id", "mac-main",
  ]);

  fs.rmSync(root, { recursive: true, force: true });
});

test("setup rejects a PEM key readable by other users", () => {
  if (process.platform === "win32") {
    return;
  }
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "tools-mcp-npm-pem-mode-"));
  const relayCa = path.join(root, "relay-ca.pem");
  const deviceCert = path.join(root, "device.pem");
  const deviceKey = path.join(root, "device.key");
  fs.writeFileSync(relayCa, "ca");
  fs.writeFileSync(deviceCert, "cert");
  fs.writeFileSync(deviceKey, "key", { mode: 0o644 });

  const result = spawnSync(process.execPath, [
    cli,
    "setup",
    "--relay-url", "wss://relay.example:8444/relay",
    "--relay-ca", relayCa,
    "--device-cert", deviceCert,
    "--device-key", deviceKey,
    "--device-id", "mac-main",
  ], {
    env: {
      ...process.env,
      TOOLS_MCP_CONFIG_FILE: path.join(root, "config.json"),
    },
    encoding: "utf8",
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /must not be accessible to group or other users/);
  fs.rmSync(root, { recursive: true, force: true });
});
