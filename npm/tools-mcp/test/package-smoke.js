"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const repository = path.resolve(__dirname, "..", "..", "..");
const artifacts = path.join(repository, "target", "npm-artifacts");
const version = require(path.join(repository, "npm", "tools-mcp", "package.json")).version;
const platformPackages = {
  "darwin-arm64": "tools-mcp-darwin-arm64",
  "darwin-x64": "tools-mcp-darwin-x64",
  "win32-x64": "tools-mcp-win32-x64",
};
const platform = `${process.platform}-${process.arch}`;
const nativePackage = platformPackages[platform];

assert.ok(nativePackage, `package smoke does not support ${platform}`);

const root = fs.mkdtempSync(path.join(os.tmpdir(), "tools-mcp-package-smoke-"));
try {
  const npmCache = path.join(root, "npm-cache");
  const metaArchive = path.join(artifacts, `tools-mcp-${version}.tgz`);
  const nativeArchive = path.join(artifacts, `${nativePackage}-${version}.tgz`);
  assert.ok(fs.statSync(metaArchive).isFile(), `missing ${metaArchive}`);
  assert.ok(fs.statSync(nativeArchive).isFile(), `missing ${nativeArchive}`);

  const install = spawnSync("npm", [
    "install",
    "--prefix", root,
    "--ignore-scripts",
    "--offline",
    "--no-audit",
    "--no-fund",
    nativeArchive,
    metaArchive,
  ], {
    encoding: "utf8",
    env: { ...process.env, npm_config_cache: npmCache },
  });
  assert.equal(install.status, 0, install.stderr || install.stdout);

  const command = path.join(
    root,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "tools-mcp.cmd" : "tools-mcp",
  );
  assert.ok(fs.statSync(command).isFile(), `missing installed command ${command}`);

  // The npm launcher passes explicit arguments straight to the packaged native
  // process. This native-only validation error proves that the full release,
  // executable mode and platform package resolution survived npm installation.
  const run = spawnSync(command, ["--bind", "0.0.0.0:8000"], {
    encoding: "utf8",
    shell: process.platform === "win32",
    windowsHide: true,
  });
  assert.notEqual(run.status, 0);
  assert.match(
    `${run.stdout}\n${run.stderr}`,
    /mcp-agent binds only to a loopback address/,
  );

  process.stdout.write(`packaged npm command reached the native ${platform} worker\n`);
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
