#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const binName = process.platform === "win32" ? "mars-xlog.exe" : "mars-xlog";
const localBin = path.join(__dirname, "..", "vendor", binName);
const configuredBin = process.env.MARS_XLOG_BIN;
const bin = configuredBin || localBin;

if (!fs.existsSync(bin)) {
  console.error(
    "mars-xlog binary is missing. Reinstall the package or set MARS_XLOG_BIN."
  );
  process.exit(127);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
