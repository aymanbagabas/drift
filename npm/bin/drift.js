#!/usr/bin/env node
// Thin launcher: exec the native drift binary that install.js placed in vendor/,
// passing through args and the terminal so the TUI works normally.
const path = require("path");
const { spawnSync } = require("child_process");

const binName = process.platform === "win32" ? "drift.exe" : "drift";
const bin = path.join(__dirname, "..", "vendor", binName);

const res = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (res.error) {
  console.error(`drift: ${res.error.message}`);
  process.exit(1);
}
process.exit(res.status ?? 0);
