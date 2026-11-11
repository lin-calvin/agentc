#!/usr/bin/env node
// Thin launcher: picks the platform package that matches this machine and execs the
// real binary. The binary itself is shipped in an optionalDependency so npm's platform
// filtering takes care of the rest.
"use strict";

const { spawn } = require("node:child_process");
const path = require("node:path");
const fs = require("node:fs");

function platformPackage() {
  if (process.platform !== "linux") {
    return { error: `agentc is a Wayland compositor and only runs on Linux (got ${process.platform})` };
  }
  const arch = { x64: "x64", arm64: "arm64" }[process.arch];
  if (!arch) return { error: `unsupported architecture: ${process.arch}` };

  // musl (Alpine) needs a separate build; glibc is what we ship today.
  const glibc = process.report?.getReport()?.header?.glibcVersionRuntime;
  if (!glibc) {
    return { error: "musl-based systems are not supported yet (glibc build only)" };
  }
  return { pkg: `agentc-mcp-linux-${arch}` };
}

function binaryPath(pkg) {
  let pkgJson;
  try {
    pkgJson = require.resolve(`${pkg}/package.json`);
  } catch {
    return null;
  }
  return path.join(path.dirname(pkgJson), "bin", "agentc");
}

function main() {
  const { pkg, error } = platformPackage();
  if (error) {
    console.error(`agentc: ${error}`);
    process.exit(1);
  }

  const bin = binaryPath(pkg);
  if (!bin) {
    console.error(
      `agentc: platform package "${pkg}" is missing.\n` +
        `Reinstall without --omit=optional, e.g.  npm i -g agentc`
    );
    process.exit(1);
  }
  if (!fs.existsSync(bin)) {
    console.error(`agentc: expected the binary at ${bin} but it is not there`);
    process.exit(1);
  }

  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => child.kill(sig));
  }
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exit(code ?? 1);
  });
}

main();
