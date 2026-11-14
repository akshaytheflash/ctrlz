#!/usr/bin/env node
"use strict";

// Thin launcher for @akshaytheflash/ctrlz. The real binary lives in a
// per-platform package that is installed as an optional dependency, so no
// download happens at install time.

const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const PLATFORMS = {
  "win32-x64": {
    package: "@akshaytheflash/ctrlz-win32-x64",
    binary: "ctrlz.exe",
  },
  "darwin-x64": {
    package: "@akshaytheflash/ctrlz-darwin-x64",
    binary: "ctrlz",
  },
  "darwin-arm64": {
    package: "@akshaytheflash/ctrlz-darwin-arm64",
    binary: "ctrlz",
  },
  "linux-x64": {
    package: "@akshaytheflash/ctrlz-linux-x64",
    binary: "ctrlz",
  },
};

const SUPPORTED = Object.keys(PLATFORMS).join(", ");

function fail(message) {
  process.stderr.write(`ctrlz: ${message}\n`);
  process.stderr.write(`ctrlz: supported platforms: ${SUPPORTED}\n`);
  process.exit(1);
}

const key = `${process.platform}-${process.arch}`;
const entry = PLATFORMS[key];

if (!entry) {
  fail(`no prebuilt binary for ${process.platform}-${process.arch}.`);
}

let packageJsonPath;
try {
  packageJsonPath = require.resolve(`${entry.package}/package.json`);
} catch {
  fail(
    `${entry.package} is not installed. It ships as an optional dependency of ` +
      `@akshaytheflash/ctrlz, which npm skips on platforms it does not support ` +
      `and which some registry mirrors do not carry. Reinstall with ` +
      `"npm install -g @akshaytheflash/ctrlz", or build from source.`,
  );
}

const binary = path.join(path.dirname(packageJsonPath), "bin", entry.binary);

if (!fs.existsSync(binary)) {
  fail(
    `${entry.package} is installed but ${binary} is missing. ` +
      `Reinstall the package, or build from source instead.`,
  );
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  fail(`failed to run ${binary}: ${result.error.message}`);
}

if (result.signal) {
  try {
    process.kill(process.pid, result.signal);
  } catch {
    process.exit(1);
  }
}

process.exit(result.status === null ? 1 : result.status);
