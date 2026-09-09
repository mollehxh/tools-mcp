#!/usr/bin/env node

"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const PLATFORM_PACKAGES = {
  "darwin-arm64": "tools-mcp-darwin-arm64",
  "darwin-x64": "tools-mcp-darwin-x64",
  "win32-x64": "tools-mcp-win32-x64",
};

function fail(message) {
  process.stderr.write(`tools-mcp: ${message}\n`);
  process.exitCode = 1;
}

function configFile() {
  if (process.env.TOOLS_MCP_CONFIG_FILE) {
    return path.resolve(process.env.TOOLS_MCP_CONFIG_FILE);
  }
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Application Support", "tools-mcp", "config.json");
  }
  if (process.platform === "win32") {
    const appData = process.env.APPDATA || path.join(os.homedir(), "AppData", "Roaming");
    return path.join(appData, "tools-mcp", "config.json");
  }
  throw new Error("the local worker supports only macOS and Windows");
}

function nativeBinary() {
  if (process.env.TOOLS_MCP_NATIVE_BINARY) {
    return path.resolve(process.env.TOOLS_MCP_NATIVE_BINARY);
  }
  const platform = `${process.platform}-${process.arch}`;
  const packageName = PLATFORM_PACKAGES[platform];
  if (!packageName) {
    throw new Error(`no native worker is published for ${platform}`);
  }
  let packageJson;
  try {
    packageJson = require.resolve(`${packageName}/package.json`);
  } catch {
    throw new Error(
      `native package ${packageName} is missing; reinstall tools-mcp with npm`,
    );
  }
  const executable = process.platform === "win32" ? "mcp-agent.exe" : "mcp-agent";
  return path.join(path.dirname(packageJson), "release", executable);
}

function parseSetup(arguments_) {
  const values = new Map();
  let force = false;
  for (let index = 0; index < arguments_.length; index += 1) {
    const flag = arguments_[index];
    if (flag === "--force") {
      force = true;
      continue;
    }
    if (!flag.startsWith("--") || index + 1 >= arguments_.length) {
      throw new Error(`setup expects a value after ${flag}`);
    }
    values.set(flag, arguments_[index + 1]);
    index += 1;
  }
  const required = ["--relay-url", "--relay-ca", "--device-cert", "--device-id"];
  for (const flag of required) {
    if (!values.has(flag)) {
      throw new Error(`setup requires ${flag}`);
    }
  }
  const allowed = [...required, "--device-key"];
  if ([...values.keys()].some((flag) => !allowed.includes(flag))) {
    const unknown = [...values.keys()].find((flag) => !allowed.includes(flag));
    throw new Error(`unknown setup option ${unknown}`);
  }
  const relayUrl = new URL(values.get("--relay-url"));
  if (relayUrl.protocol !== "wss:") {
    throw new Error("relay URL must use wss://");
  }
  const deviceId = values.get("--device-id");
  if (!/^[A-Za-z0-9_.-]{1,128}$/.test(deviceId)) {
    throw new Error("device ID may contain only letters, digits, dot, dash, and underscore");
  }
  const relayCa = fs.realpathSync(values.get("--relay-ca"));
  const deviceCert = fs.realpathSync(values.get("--device-cert"));
  for (const file of [relayCa, deviceCert]) {
    if (!fs.statSync(file).isFile()) {
      throw new Error(`credential path is not a file: ${file}`);
    }
  }
  let deviceKey;
  if (values.has("--device-key")) {
    deviceKey = fs.realpathSync(values.get("--device-key"));
    const metadata = fs.statSync(deviceKey);
    if (!metadata.isFile()) {
      throw new Error(`credential path is not a file: ${deviceKey}`);
    }
    if (process.platform !== "win32" && (metadata.mode & 0o077) !== 0) {
      throw new Error("device key must not be accessible to group or other users");
    }
  }
  return {
    force,
    config: { relayUrl: relayUrl.href, relayCa, deviceCert, deviceId, deviceKey },
  };
}

function saveConfig(arguments_) {
  const { force, config } = parseSetup(arguments_);
  const destination = configFile();
  if (!force && fs.existsSync(destination)) {
    throw new Error(`configuration already exists at ${destination}; use --force to replace it`);
  }
  const directory = path.dirname(destination);
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  const temporary = `${destination}.${process.pid}.tmp`;
  const serialized = `${JSON.stringify(config, null, 2)}\n`;
  fs.writeFileSync(temporary, serialized, { encoding: "utf8", mode: 0o600, flag: "wx" });
  fs.renameSync(temporary, destination);
  process.stdout.write(`Saved tools-mcp configuration to ${destination}\n`);
}

function configuredArguments() {
  const source = configFile();
  let config;
  try {
    config = JSON.parse(fs.readFileSync(source, "utf8"));
  } catch (error) {
    if (error && error.code === "ENOENT") {
      throw new Error(`not configured; run tools-mcp setup first`);
    }
    throw new Error(`cannot read configuration ${source}`);
  }
  const required = ["relayUrl", "relayCa", "deviceCert", "deviceId"];
  if (required.some((key) => typeof config[key] !== "string" || config[key].length === 0)) {
    throw new Error(`configuration ${source} is incomplete`);
  }
  const keyArguments = config.deviceKey
    ? ["--device-key", config.deviceKey]
    : process.platform === "win32"
      ? ["--device-cng-key-name", `tools-mcp-device:${config.deviceId}`]
      : ["--device-keychain-label", `tools-mcp-device:${config.deviceId}`];
  return [
    "--relay-url", config.relayUrl,
    "--relay-ca", config.relayCa,
    "--device-cert", config.deviceCert,
    ...keyArguments,
    "--device-id", config.deviceId,
  ];
}

function launch(arguments_) {
  const child = spawn(nativeBinary(), arguments_, {
    cwd: process.cwd(),
    env: process.env,
    stdio: "inherit",
    windowsHide: false,
  });
  child.once("error", (error) => fail(`cannot start native worker: ${error.message}`));
  child.once("exit", (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
    } else {
      process.exitCode = code ?? 1;
    }
  });
}

try {
  const arguments_ = process.argv.slice(2);
  if (arguments_[0] === "setup") {
    saveConfig(arguments_.slice(1));
  } else {
    launch(arguments_.length === 0 ? configuredArguments() : arguments_);
  }
} catch (error) {
  fail(error instanceof Error ? error.message : String(error));
}
