'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');

const MAX_BINARY_BYTES = 300 * 1024 * 1024;
const PLATFORM_ASSETS = {
  'linux:x64': 'wrosecode-linux-x86_64',
  'linux:arm64': 'wrosecode-linux-aarch64',
  'darwin:x64': 'wrosecode-darwin-x86_64',
  'darwin:arm64': 'wrosecode-darwin-aarch64',
  'win32:x64': 'wrosecode-windows-x86_64.exe',
  'win32:arm64': 'wrosecode-windows-aarch64.exe'
};

function assetName(platform = process.platform, arch = process.arch) {
  const name = PLATFORM_ASSETS[`${platform}:${arch}`];
  if (!name) {
    throw new Error(`Unsupported platform ${platform}/${arch}. Supported targets: Linux, macOS, and Windows on x86_64 or aarch64.`);
  }
  return name;
}

function releaseBase(pkg) {
  if (process.env.WROSECODE_RELEASE_BASE) return process.env.WROSECODE_RELEASE_BASE.replace(/\/+$/, '');

  const configured = pkg.repository?.url;
  if (typeof configured !== 'string') throw new Error('package.json is missing repository.url; cannot locate the native release.');
  const normalized = configured.replace(/^git\+/, '').replace(/\.git$/, '');
  let repository;
  try {
    repository = new URL(normalized);
  } catch {
    throw new Error('package.json repository.url is not a valid HTTPS GitHub URL.');
  }
  if (repository.protocol !== 'https:' || repository.hostname.toLowerCase() !== 'github.com' || repository.pathname.toLowerCase() !== '/whiterose717/wrosecode-cli') {
    throw new Error('package.json repository.url must identify https://github.com/whiterose717/WROSECODE-CLI.git.');
  }
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(pkg.version)) throw new Error(`Invalid package version: ${pkg.version}`);
  return `${repository.origin}${repository.pathname}/releases/download/v${pkg.version}`;
}

function userCacheBinaryPath(version, platform = process.platform) {
  const binaryName = platform === 'win32' ? 'wrosecode.exe' : 'wrosecode';
  if (platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
    return path.join(localAppData, 'WROSECODE', version, binaryName);
  }
  const cacheHome = process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache');
  return path.join(cacheHome, 'wrosecode', version, binaryName);
}

async function fetchHttps(url) {
  const parsed = new URL(url);
  if (parsed.protocol !== 'https:') throw new Error('Release downloads must use HTTPS.');
  const response = await fetch(parsed, { signal: AbortSignal.timeout(120_000), redirect: 'follow' });
  if (!response.ok) throw new Error(`Download failed (${response.status} ${response.statusText}): ${parsed}`);
  if (new URL(response.url).protocol !== 'https:') throw new Error('Release download redirected to a non-HTTPS URL.');
  return response;
}

async function readLimited(response, limit) {
  if (!response.body) throw new Error('Release server returned an empty response body.');
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > limit) {
      await reader.cancel();
      throw new Error('Downloaded release asset exceeds the allowed size.');
    }
    chunks.push(Buffer.from(value));
  }
  return Buffer.concat(chunks, size);
}

async function installBinary() {
  // Explicit override is intended for local builds and pack/install smoke tests.
  if (process.env.WROSECODE_BINARY) return;

  const pkg = require('../package.json');
  const base = releaseBase(pkg);
  const name = assetName();
  const assetUrl = `${base}/${name}`;

  const binary = await readLimited(await fetchHttps(assetUrl), MAX_BINARY_BYTES);
  if (binary.length === 0) throw new Error(`Release asset is empty: ${name}`);

  const checksumBytes = await readLimited(await fetchHttps(`${assetUrl}.sha256`), 1024);
  const checksumText = checksumBytes.toString('utf8').trim();
  const checksumMatch = /^([a-fA-F0-9]{64})(?:\s+\*?([^\s]+))?$/.exec(checksumText);
  if (!checksumMatch || (checksumMatch[2] && path.basename(checksumMatch[2]) !== name)) {
    throw new Error(`Invalid SHA-256 sidecar for ${name}.`);
  }
  const expected = checksumMatch[1].toLowerCase();
  const actual = crypto.createHash('sha256').update(binary).digest('hex');
  if (actual !== expected) throw new Error(`SHA-256 verification failed for ${name}.`);

  const binDir = path.join(__dirname, 'bin');
  const destination = path.join(binDir, process.platform === 'win32' ? 'wrosecode.exe' : 'wrosecode');
  try {
    await writeBinary(destination, binary);
  } catch (error) {
    // Global installs may be root-owned while npm lifecycle scripts are
    // disabled. Keep the executable in the user's cache in that case.
    if (!['EACCES', 'EPERM', 'EROFS'].includes(error.code)) throw error;
    await writeBinary(userCacheBinaryPath(pkg.version), binary);
  }
}

async function writeBinary(destination, binary) {
  const destinationDir = path.dirname(destination);
  const temporary = `${destination}.tmp-${process.pid}`;
  await fs.mkdir(destinationDir, { recursive: true, mode: 0o700 });
  try {
    await fs.writeFile(temporary, binary, { mode: 0o755, flag: 'wx' });
    if (process.platform !== 'win32') await fs.chmod(temporary, 0o755);
    try {
      await fs.rename(temporary, destination);
    } catch (error) {
      if (process.platform !== 'win32' || !['EEXIST', 'EPERM', 'ENOTEMPTY'].includes(error.code)) throw error;
      await fs.rm(destination, { force: true });
      await fs.rename(temporary, destination);
    }
  } finally {
    await fs.rm(temporary, { force: true });
  }
}

if (require.main === module) {
  installBinary().catch((error) => {
    console.error(`wrosecode: native binary installation failed: ${error.message}`);
    process.exitCode = 1;
  });
}

module.exports = { assetName, releaseBase, userCacheBinaryPath };
