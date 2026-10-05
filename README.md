# WROSECODE

<p align="center">
  <strong>A fast, native AI coding agent for the terminal — built in Rust.</strong>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/wrosecode"><img src="https://img.shields.io/npm/v/wrosecode?logo=npm&label=npm" alt="npm version"></a>
  <a href="https://github.com/whiterose717/WROSECODE-CLI"><img src="https://img.shields.io/badge/source-GitHub-181717?logo=github" alt="GitHub"></a>
  <img src="https://img.shields.io/badge/Rust-native-000000?logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-blue" alt="Platforms">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="MIT License">
</p>

---

## Overview

**WROSECODE** is a terminal-first AI coding agent written in Rust.

It combines a responsive full-screen TUI, streamed model responses, repository-aware context, file editing, shell execution, persistent sessions, provider management, MCP support, CTF workflows, automation, and configurable permission modes directly from the terminal.

---

## Installation

### Install from npm

Standard global installation:

```bash
npm install -g wrosecode
```

Then run:

```bash
wrosecode
```

Check the installed version:

```bash
wrosecode --version
```

### If npm blocks install scripts

WROSECODE currently uses an npm install script to download and verify the correct native binary for your platform.

If npm shows an `install scripts blocked` warning, install with:

```bash
npm install -g --allow-scripts=wrosecode wrosecode
```

To permanently allow WROSECODE install scripts for your user account:

```bash
npm config set allow-scripts=wrosecode --location=user
```

Then future installs can use:

```bash
npm install -g wrosecode
```

### Run without permanent installation

```bash
npx wrosecode
```

### Update

```bash
npm install -g --allow-scripts=wrosecode wrosecode@latest
```

### Uninstall

```bash
npm uninstall -g wrosecode
```

---

## Quick Start

Start WROSECODE in the current project directory:

```bash
wrosecode
```

Run a single task:

```bash
wrosecode "explain this repository and identify the main entry points"
```

Run with YOLO permissions:

```bash
wrosecode --permission yolo
```

Run a task directly with YOLO permissions:

```bash
wrosecode --permission yolo "fix the failing tests and explain the changes"
```

---

## Permission Modes

WROSECODE supports multiple permission levels.

| Mode | Behavior |
|---|---|
| `ask` | Ask before mutating or risky operations |
| `auto-safe` | Automatically run safer actions and prompt for riskier actions |
| `yolo` | Skip most normal approval prompts and allow more autonomous tool execution |

Conservative mode:

```bash
wrosecode --permission ask
```

Automatic safe mode:

```bash
wrosecode --permission auto-safe
```

YOLO mode:

```bash
wrosecode --permission yolo
```

> **Warning:** `--permission yolo` gives WROSECODE significantly more freedom to execute tools and modify files. Use it only in directories and environments where you are comfortable with autonomous changes.

---

## How the npm Package Works

The npm package is a small launcher. During installation it:

1. Detects your operating system and CPU architecture.
2. Downloads the matching native WROSECODE binary from the corresponding GitHub Release.
3. Downloads the matching `.sha256` checksum.
4. Verifies the binary.
5. Installs the native executable.
6. Launches it when you run `wrosecode`.

Rust and Cargo are **not required** for npm users.

### Supported Platforms

| Platform | Architecture |
|---|---|
| Linux | x86_64 |
| Linux | aarch64 |
| macOS | x86_64 |
| macOS | aarch64 / Apple Silicon |
| Windows | x86_64 |
| Windows | aarch64 |

Node.js 18 or newer is required for npm installation.

---

## Core Features

- Native Rust CLI
- Full-screen terminal UI
- Streamed AI responses
- Multiline prompt input
- Repository-aware context
- File reading and editing
- Shell and tool execution
- Persistent sessions
- Session resume and fork
- Prompt history
- Multiple model/provider backends
- Thinking levels
- MCP support
- Skills
- Custom commands
- Custom agents
- Recipes
- Headless automation
- Machine-readable event streams
- Live dashboard
- Docker sandbox support
- CTF workflows
- CTFd integration
- SHA-256 verified native releases

---

## Providers

WROSECODE supports built-in and custom providers, including:

- Anthropic
- OpenAI-compatible APIs
- Ollama
- LM Studio
- Custom OpenAI-compatible gateways
- Custom Anthropic-compatible gateways

Open the provider manager:

```text
/providers
```

List providers:

```bash
wrosecode providers list
```

Add a custom provider:

```bash
printf '%s\n' "$MY_API_KEY" | wrosecode providers add my-gateway \
  --base-url https://llm.example.com/v1 \
  --stdin \
  --model my-model
```

Test a provider:

```bash
wrosecode providers test my-gateway
```

Use a provider:

```bash
wrosecode --provider my-gateway "Explain this project"
```

---

## Thinking Levels

WROSECODE supports:

```text
off → low → medium → high → max → auto
```

Cycle thinking levels:

```text
Ctrl+T
```

Set one directly:

```bash
wrosecode --think high
```

---

## Keyboard Controls

| Key | Action |
|---|---|
| `Enter` | Submit prompt |
| `Shift+Enter` | Insert newline |
| `PageUp` / `PageDown` | Scroll transcript |
| `Home` / `End` | Move caret or jump through transcript |
| `Ctrl+T` | Cycle thinking level |
| `Ctrl+D` | Open/close live dashboard when prompt is empty |
| `Ctrl+P` | Open command palette |
| `Ctrl+Shift+C` | Copy latest output |
| `Ctrl+K` | Clear input |
| `Ctrl+W` | Delete previous word |
| `Ctrl+A` / `Ctrl+E` | Jump to start/end of line |
| `Tab` | Complete input or switch build/plan mode |
| `Ctrl+C` | Cancel current turn, clear input, or exit with confirmation |

---

## Live Dashboard

Open the dashboard:

```text
Ctrl+D
```

or:

```text
/dashboard
```

The dashboard includes:

- Processes
- Thinking
- Timeline
- Plan
- Tokens & Cost
- Files
- CTF
- Budget

Outside the TUI:

```bash
wrosecode dashboard
```

Plain output:

```bash
wrosecode dashboard --once --offline
```

Headless JSON:

```bash
wrosecode exec --json "inspect this repository"
```

---

## CTF Mode

WROSECODE includes workflows for:

- Web
- Pwn
- Crypto
- Reverse engineering
- Forensics
- Steganography
- OSINT
- Network analysis

Run:

```bash
wrosecode ctf challenge.pcap --budget 60
```

Example:

```bash
wrosecode ctf chal.txt \
  --flag-format 'SECRET\{[^}]+\}' \
  --category forensics
```

CTFd helpers:

```bash
wrosecode ctfd list
wrosecode ctfd scoreboard
wrosecode ctfd download 42
wrosecode ctfd submit 42 'flag{answer}'
wrosecode ctfd verify 42 'flag{answer}'
```

---

## Sessions

Resume a session:

```bash
wrosecode --session SESSION_ID
```

Short form:

```bash
wrosecode -s SESSION_ID
```

Fork:

```bash
wrosecode --session SESSION_ID --fork
```

Export:

```bash
wrosecode --session SESSION_ID --export-session session.json
```

Import:

```bash
wrosecode --import-session session.json
```

---

## MCP

Attach an MCP stdio server:

```bash
wrosecode --mcp-bin PATH --mcp-arg ARG
```

Manage saved MCP servers:

```text
/mcps
```

---

## Headless & Automation

Run without the full-screen TUI:

```bash
wrosecode --headless --summary json "analyze this repository"
```

Write structured events:

```bash
wrosecode --headless \
  --events run.jsonl \
  --summary json \
  "analyze the supplied challenge"
```

Start local web/API mode:

```bash
wrosecode --web
```

---

## Docker Sandbox

Example `config.toml`:

```toml
[sandbox]
engine = "docker"
image = "wrosecode-sandbox:latest"
persistent = true
name = "wrosecode-sandbox"
network = "bridge"
memory = "2g"
cpus = "2.0"
```

Build:

```bash
docker build -f Dockerfile.sandbox -t wrosecode-sandbox:latest .
```

Useful commands:

```text
/sandbox
/sandbox up
/sandbox down
```

---

## Build From Source

Clone:

```bash
git clone https://github.com/whiterose717/WROSECODE-CLI.git
cd WROSECODE-CLI
```

Build:

```bash
cargo build --release
```

Run:

```bash
./target/release/wrosecode
```

Install with Cargo:

```bash
cargo install --path .
```

---

## Testing

Formatting:

```bash
cargo fmt --check
```

Clippy:

```bash
cargo clippy --all-targets -- -D warnings
```

Tests:

```bash
cargo test --all-targets
```

Release benchmark regression tests:

```bash
cargo test --release benchmarks -- --nocapture
```

---

## Security

- Never commit `.env` files, API keys, npm tokens, GitHub tokens, passwords, or private keys.
- Prefer environment variables, stdin-based secret entry, or built-in credential storage.
- Use `--permission ask` for conservative workflows.
- Use Docker sandboxing for stronger shell isolation.
- Treat repository content, tool output, and web content as untrusted input.
- Native release binaries are verified using SHA-256 checksums.

---

## MIT License

WROSECODE is released under the **MIT License**.

```text
MIT License

Copyright (c) WROSECODE contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

See [`LICENSE`](LICENSE) for the full license text.

---

## Links

- **GitHub:** https://github.com/whiterose717/WROSECODE-CLI
- **npm:** https://www.npmjs.com/package/wrosecode
- **Releases:** https://github.com/whiterose717/WROSECODE-CLI/releases
- **Issues:** https://github.com/whiterose717/WROSECODE-CLI/issues

---

<p align="center">
  <strong>WROSECODE</strong><br>
  Native AI coding workflows for the terminal.
</p>
