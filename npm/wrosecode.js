#!/usr/bin/env node
'use strict';

const { spawn, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const pkg = require('../package.json');
const { userCacheBinaryPath } = require('./install.js');
const packagedBinary = path.join(__dirname, 'bin', process.platform === 'win32' ? 'wrosecode.exe' : 'wrosecode');
const cachedBinary = userCacheBinaryPath(pkg.version);
let binary = process.env.WROSECODE_BINARY || [packagedBinary, cachedBinary].find((candidate) => fs.existsSync(candidate)) || null;

if (!binary) {
  // npm can be configured to skip lifecycle scripts. Install lazily on first run
  // in that case, but fail instead of launching without a native executable.
  const installer = spawnSync(process.execPath, [path.join(__dirname, 'install.js')], { stdio: 'inherit' });
  if (installer.error) {
    console.error(`wrosecode: could not run native installer: ${installer.error.message}`);
    process.exit(1);
  }
  if (installer.status !== 0) process.exit(installer.status ?? 1);
  binary = [packagedBinary, cachedBinary].find((candidate) => fs.existsSync(candidate)) || null;
}

if (!binary) {
  console.error('wrosecode: native executable is missing after installation. Reinstall the package or set WROSECODE_BINARY to an executable path.');
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: true
});

let signalForwarded = false;
const signalsToForward = process.stdin.isTTY && process.stdout.isTTY ? ['SIGTERM'] : ['SIGINT', 'SIGTERM'];
for (const signal of signalsToForward) {
  process.on(signal, () => {
    if (signalForwarded || child.exitCode !== null || child.signalCode !== null || child.pid === undefined) return;
    signalForwarded = true;
    try {
      child.kill(signal);
    } catch (error) {
      if (error.code !== 'ESRCH') {
        console.error(`wrosecode: could not forward ${signal} to native process: ${error.message}`);
      }
    }
  });
}

let spawnFailed = false;
child.once('error', (error) => {
  spawnFailed = true;
  console.error(`wrosecode: could not start native executable: ${error.message}`);
  process.exitCode = 1;
});

child.once('close', (code, signal) => {
  if (spawnFailed) return;
  if (code !== null) {
    process.exitCode = code;
  } else if (signal) {
    process.exitCode = 128 + (os.constants.signals[signal] || 1);
  } else if (process.exitCode === undefined) {
    process.exitCode = 1;
  }
});
