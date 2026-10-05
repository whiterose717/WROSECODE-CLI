'use strict';

const fs = require('node:fs');
const path = require('node:path');
const pkg = require('../package.json');
const lock = require('../package-lock.json');
const cargoToml = fs.readFileSync(path.resolve(__dirname, '../Cargo.toml'), 'utf8');
const installer = require('./install.js');

const root = path.resolve(__dirname, '..');
const errors = [];
const expectedRepo = 'https://github.com/whiterose717/WROSECODE-CLI.git';
const expectedHomepage = 'https://github.com/whiterose717/WROSECODE-CLI#readme';
const expectedBugs = 'https://github.com/whiterose717/WROSECODE-CLI/issues';
const expectedFiles = [
  'npm/wrosecode.js',
  'npm/install.js',
  'npm/check-publish.js',
  'LICENSE',
  'README.md'
];
const requiredAssets = [
  ['linux', 'x64', 'wrosecode-linux-x86_64'],
  ['linux', 'arm64', 'wrosecode-linux-aarch64'],
  ['darwin', 'x64', 'wrosecode-darwin-x86_64'],
  ['darwin', 'arm64', 'wrosecode-darwin-aarch64'],
  ['win32', 'x64', 'wrosecode-windows-x86_64.exe'],
  ['win32', 'arm64', 'wrosecode-windows-aarch64.exe']
];

function requireCondition(condition, message) {
  if (!condition) errors.push(message);
}

requireCondition(pkg.name === 'wrosecode', 'package name must be "wrosecode".');
requireCondition(/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(pkg.version || ''), 'package version must use semantic versioning.');
requireCondition(pkg.version === lock.version && lock.packages?.['']?.version === pkg.version, 'package-lock.json version does not match package.json.');
requireCondition(new RegExp(`^version\\s*=\\s*"${pkg.version.replaceAll('.', '\\.')}"$`, 'm').test(cargoToml), 'Cargo.toml version does not match package.json.');
requireCondition(typeof pkg.description === 'string' && pkg.description.trim().length > 0, 'package description is required.');
requireCondition(pkg.license === 'MIT', 'package license must be MIT.');
requireCondition(fs.existsSync(path.join(root, 'LICENSE')) && fs.readFileSync(path.join(root, 'LICENSE'), 'utf8').startsWith('MIT License'), 'MIT LICENSE file is missing or invalid.');
requireCondition(Array.isArray(pkg.keywords) && pkg.keywords.length > 0, 'at least one package keyword is required.');
requireCondition(pkg.engines?.node === '>=18', 'Node.js engine must be >=18.');
requireCondition(pkg.bin?.wrosecode === 'npm/wrosecode.js', 'wrosecode bin must point to npm/wrosecode.js.');
requireCondition(pkg.repository?.type === 'git' && pkg.repository?.url === expectedRepo, `repository.url must be ${expectedRepo}.`);
requireCondition(pkg.homepage === expectedHomepage, `homepage must be ${expectedHomepage}.`);
requireCondition(pkg.bugs?.url === expectedBugs, `bugs.url must be ${expectedBugs}.`);
requireCondition(JSON.stringify(pkg.files) === JSON.stringify(expectedFiles), 'package files allowlist must contain only the five approved npm distribution files.');

for (const relativePath of expectedFiles) {
  const file = path.join(root, relativePath);
  requireCondition(fs.existsSync(file) && fs.statSync(file).isFile(), `required package file is missing: ${relativePath}`);
}

const launcherPath = path.join(root, 'npm/wrosecode.js');
if (fs.existsSync(launcherPath)) {
  const launcher = fs.readFileSync(launcherPath, 'utf8');
  requireCondition(launcher.startsWith('#!/usr/bin/env node\n'), 'npm/wrosecode.js must have the Node.js executable shebang.');
}

const installerPath = path.join(root, 'npm/install.js');
let installerSource = '';
if (fs.existsSync(installerPath)) {
  installerSource = fs.readFileSync(installerPath, 'utf8');
  for (const [platform, arch, expectedAsset] of requiredAssets) {
    requireCondition(installer.assetName(platform, arch) === expectedAsset, `installer asset mapping is incorrect for ${platform}/${arch}.`);
  }
  const previousOverride = process.env.WROSECODE_RELEASE_BASE;
  delete process.env.WROSECODE_RELEASE_BASE;
  try {
    requireCondition(installer.releaseBase(pkg) === `https://github.com/whiterose717/WROSECODE-CLI/releases/download/v${pkg.version}`, 'installer release URL must resolve from repository metadata and package version.');
  } catch (error) {
    errors.push(`installer cannot derive the release URL: ${error.message}`);
  } finally {
    if (previousOverride === undefined) delete process.env.WROSECODE_RELEASE_BASE;
    else process.env.WROSECODE_RELEASE_BASE = previousOverride;
  }
  requireCondition(installerSource.includes("protocol !== 'https:'") && installerSource.includes("createHash('sha256')") && installerSource.includes('actual !== expected'), 'installer must enforce HTTPS and SHA-256 verification.');
  requireCondition(!/(?:YOUR_GITHUB|YOUR_REPOSITORY|example\.com|placeholder|TODO_RELEASE_URL)/i.test(installerSource), 'installer contains a placeholder release URL or TODO.');
}

const readmePath = path.join(root, 'README.md');
if (fs.existsSync(readmePath)) {
  const readme = fs.readFileSync(readmePath, 'utf8');
  requireCondition(readme.includes(expectedRepo.replace(/\.git$/, '')), 'README must link to the actual GitHub repository.');
  requireCondition(!/(?:<YOUR_GITHUB_REPOSITORY_URL>|github\.com\/(?:example|your-org|your-user)\/)/i.test(readme), 'README contains a placeholder repository URL.');
  for (const command of [
    'npm install -g wrosecode',
    'wrosecode',
    'npx wrosecode',
    'npm install -g wrosecode@latest',
    'npm uninstall -g wrosecode'
  ]) requireCondition(readme.includes(command), `README is missing the npm command: ${command}`);
}

// Scan only files that npm is explicitly permitted to publish. Never emit a
// matching line or credential value in diagnostics.
const credentialPatterns = [
  /-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----/,
  /\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|npm_[A-Za-z0-9]{30,}|AKIA[0-9A-Z]{16})\b/,
  /\bsk-(?:ant|proj|live|svc)?-[A-Za-z0-9_-]{32,}\b/i,
  /\b(?:ANTHROPIC_API_KEY|OPENAI_API_KEY|NPM_TOKEN|GITHUB_TOKEN|CTFD_TOKEN)\s*[:=]\s*["']?([^\s"'#]{24,})/i
];
for (const relativePath of expectedFiles) {
  const file = path.join(root, relativePath);
  if (!fs.existsSync(file) || !fs.statSync(file).isFile()) continue;
  const content = fs.readFileSync(file, 'utf8');
  if (credentialPatterns.some((pattern) => pattern.test(content))) {
    errors.push(`possible hard-coded credential detected in npm package file: ${relativePath}`);
  }
}

async function verifyPublishedRelease() {
  const endpoint = `https://api.github.com/repos/whiterose717/WROSECODE-CLI/releases/tags/v${pkg.version}`;
  let response;
  try {
    response = await fetch(endpoint, {
      headers: {
        accept: 'application/vnd.github+json',
        'x-github-api-version': '2022-11-28',
        'user-agent': 'wrosecode-npm-publish-check'
      },
      signal: AbortSignal.timeout(20_000)
    });
  } catch (error) {
    errors.push(`could not verify GitHub Release v${pkg.version}: ${error.message}`);
    return;
  }
  if (!response.ok) {
    errors.push(`GitHub Release v${pkg.version} is not publicly available (HTTP ${response.status}); push the matching tag and wait for the release workflow before publishing.`);
    return;
  }

  let release;
  try {
    release = await response.json();
  } catch {
    errors.push('GitHub returned invalid release metadata; cannot verify native download assets.');
    return;
  }
  requireCondition(release.tag_name === `v${pkg.version}` && !release.draft && !release.prerelease, `GitHub Release v${pkg.version} must be published (not draft or prerelease).`);
  const availableAssets = new Set((release.assets || []).map((asset) => asset.name));
  const missingAssets = requiredAssets.flatMap(([, , asset]) => [asset, `${asset}.sha256`]).filter((asset) => !availableAssets.has(asset));
  requireCondition(missingAssets.length === 0, `GitHub Release v${pkg.version} is missing assets: ${missingAssets.join(', ')}.`);
}

if (errors.length) {
  console.error('npm publish preflight failed:');
  for (const error of errors) console.error(`- ${error}`);
  process.exit(1);
}

verifyPublishedRelease().then(() => {
  if (errors.length) {
    console.error('npm publish preflight failed:');
    for (const error of errors) console.error(`- ${error}`);
    process.exitCode = 1;
    return;
  }
  console.log(`npm publish preflight passed for ${pkg.name}@${pkg.version}; matching public release assets are present.`);
}).catch((error) => {
  console.error(`npm publish preflight failed: ${error.message}`);
  process.exitCode = 1;
});
