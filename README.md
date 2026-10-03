# WROSECODE

A terminal coding agent written in Rust. Build with `cargo install --path .` or `cargo build --release`. Run `wrosecode` from a project directory, or `wrosecode 'your task'` for one turn. Set `ANTHROPIC_API_KEY` for the Anthropic provider. For an OpenAI-compatible server, set `WROSECODE_OPENAI_URL` and run `wrosecode --provider openai-compat --model MODEL`.

## Install and run

```bash
cd /home/kali/Desktop/wrose
cargo build --release
install -Dm755 target/release/wrosecode ~/.local/bin/wrosecode
wrosecode --permission yolo
```

Rust was chosen for low input latency, predictable memory use, a single native binary, and direct terminal control. The architecture combines a differential TUI, streamed provider adapters, durable sessions, MCP tools, repository maps, skills, permission tiers, and parallel tool execution.

## CTF dashboard

The first view is an opencode-style start screen: the `WROSECODE` wordmark, a
live spinner, the session ID, the challenge category, the speed tier the current
thinking level maps to, and the keybinding hints. It only draws while the
transcript is still empty — as soon as you type or a turn produces output it
folds back into the header so the conversation gets the screen.

Captured at 110×36 straight from a real run:

```
╦ ╦╔╗ ╭─╮╔═╗╔═╗╭─╮╭─╮╔═╗╔═╗  ⠧  session 1791039835-413394-0 · [misc]
║ ║╠╩╗│ │╠═╝╠═╗│  │ │║ ║╠═╗
╚═╝╚═╝╰─╯╚═╝╚═╝╰─╯╰─╯╚═╝╚═╝  WROSECODE v0.2.0 · FAST · /home/kali/Desktop/wrose
 BUILD • anthropic / claude-sonnet-5 • claude • ask • [misc] • think 5 · up to 3 agents
──────────────────────────────────────────────────────────────────────────────────────────────────────────────
 TRANSCRIPT  1 entry                                                      │ TOKEN DASHBOARD
 INFO  WROSECODE v0.2.0 · session 1791039835-413394-0                     │ input         0 ░░░░░░░░░░░░░░░░
       Type a request · /help commands · Ctrl+P palette                   │ output        0 ░░░░░░░░░░░░░░░░
       Tab build/plan · [ ] thinking · Esc then j/k to scroll             │ reason        0 ░░░░░░░░░░░░░░░░
                                                                          │ cache R       0 ░░░░░░░░░░░░░░░░
                                                                          │ cache W       0 ░░░░░░░░░░░░░░░░
                                                                          │ model  claude-sonnet-5
                                                                          │ cost   n/a
                                                                          │ budget unlimited
                                                                          │ p50/p95/p99 0/0/0ms
                                                                          │ cache  0%
                                                                          │ flags  0
                                                                          │ turn   ·
                                                                          │ errors 0
                                                                          │ tier   FAST · 3 workers
                                                                          │
                                                                          │
                                                                          │
 SUBAGENT TREE  0 active                                                  │ TOOL TIMELINE  0 calls
 root [misc] think 5/20 · up to 3 agents                                  │
                                                                          │
                                                                          │
                                                                          │
                                                                          │
                                                                          │
                                                                          │
                                                                          │
                                                                          │
 Ready • FAST · 3w • tok 0/0 • cost n/a • cache 0% • tools 0:0 • errors 0 • flags 0 • 3s • think 5
──────────────────────────────────────────────────────────────────────────────────────────────────────────────
>
```

Rows 7–9 are the welcome entry in the transcript; rows 1–4 are the rendered
header drawn above it. The header is what the spinner animates in, and it
carries `TURBO` / `FAST` / `SMART` / `DEEP` plus the worker count that tier
actually allows (1 / 3 / 5 / 10).

The right-hand dashboard shows input, output, reasoning, and cache traffic as
bars, then the model, cost, budget, latency percentiles, cache hit rate, found
flags, a per-turn token sparkline with the last turn duration, an error counter
with the kind of the most recent failure, and the speed tier. Once a turn ends
the status line reads `Ready` and keeps `tools`/`errors`/`flags` totals. During
a turn the status line swaps in an animated spinner and the live token counts.

File magic and extensions categorize common web, pwn, crypto, reverse
engineering, and forensics tasks.

A found flag raises a `🚩 FLAG ALERT` banner under the header and survives until
the next turn or `/clear`. It also emits a terminal bell (`\x07`, disabled with
`alert_bell = false`) and a desktop notification when `notify-send` is available,
so a flag found in a background pane is not missed.

Every model response and tool result is scanned for `flag{...}`, `CTF{...}`, `picoCTF{...}`, and `HTB{...}`. The patterns live in `config.toml` under `[ctf]`; `auto_copy` controls the clipboard copy. New flags are highlighted and appended to `.ctf/flags.log`. Optional CTFd submission is enabled with `auto_submit = true` in `config.toml`, or from the environment:

```bash
export WROSECODE_CTFD_AUTO_SUBMIT=1   # overrides config.toml
export CTFD_URL=https://ctf.example
export CTFD_TOKEN=your-token
export CTFD_CHALLENGE_ID=42
```

CTF skills cover recon, web, pwn, crypto, reverse engineering, forensics, stego, OSINT, and network analysis. Independent model tool calls run concurrently; `delegate_task` supports provider and model routing. Identical read-only calls use the in-memory result cache, while file edits invalidate cached results.

### Keyboard controls

| Key | Action |
| --- | --- |
| `↑` / `↓` | Scroll the transcript whenever the input box is empty |
| `PageUp` / `PageDown` | Move the transcript a full viewport at a time (at the real page size, not a fixed five lines) |
| `Home` / `End` | Jump to the top or bottom of the transcript when the input is empty; otherwise move the caret |
| `Ctrl+Home` / `Ctrl+End` | Always jump to the top or bottom of the transcript |
| `Esc`, then `j` / `k` | Enter navigation mode and scroll with the single keys; `Space`/`b` page down/up |
| `Shift+Enter` | Insert a newline |
| `Enter` | Submit the prompt or selected command; during a turn it queues the message for after the reply |
| `Ctrl+K` | Clear input |
| `Ctrl+U` | Clear input when there is text, otherwise scroll half a page up |
| `Ctrl+D` | Delete the character under the caret when there is text, otherwise scroll half a page down |
| `Ctrl+W` | Delete the word before the caret |
| `Ctrl+A` / `Ctrl+E` | Jump to the start / end of the line |
| `Alt+←` / `Alt+→` (also `Alt+B` / `Alt+F`) | Jump a word left / right |
| `Ctrl+L` | Clear the visible transcript and redraw (history and conversation context are kept; `/clear --context` drops the context too) |
| `Ctrl+T` | Cycle the color theme (same list as `/theme`) |
| `Ctrl+Shift+C` | Copy latest output |
| `[` / `]` | Lower or raise thinking level (0–20) |
| `@` at the start of a prompt | Open the file-mention picker (`Mention files`); `Esc` closes it |
| Bracketed paste | A paste of 4+ lines collapses into `[Pasted N lines]`: `Enter` inserts it, `Esc` discards it |
| `Tab` | Complete the current word (paths, slash commands, tool names) when the input is not empty; otherwise switch build and plan modes |
| `Ctrl+C` | Clear what you are typing, then exit on a second press. During a turn it cancels the turn instead of closing the shell |
| Mouse wheel | Scroll the transcript (smooth scrolling follows `smooth_scroll_lines`) |

While scrolled up, the transcript header shows `· ↓ N new lines  (End to jump)`; each new line arriving is counted there until `End` returns to the bottom. Multi-line drafts grow the input box to four rows and then scroll with the caret. Prompt history (last 500 entries, consecutive duplicates collapsed) persists to `~/.wrosecode/history` and is recalled with `↑`/`↓`.

Shell calls have a 30 second timeout and are terminated if stuck. Panics and turn errors are written to `.ctf/errors.log`; the TUI keeps provider errors visible with a recovery suggestion. One-shot runs print a computed usage report, and `--summary json` emits a machine-readable `TaskComplete` object.

## Advanced dashboard and persistence

The terminal dashboard has four live panes: transcript, token and cost metrics, subagent tree, and tool timeline. Drag the vertical separator with the mouse. `Ctrl+T` cycles dark, light, solarized, Dracula, and Nord themes; `/theme` opens a picker and `/theme NAME` applies one directly (`/theme` on an unknown name lists what exists). The shell paints its first frame — wordmark, version, and session line — before provider setup, skill discovery, MCP reconnects, and session restore, so startup never waits on the network: the measured time is stamped next to the session line (`· paint 9ms`), reported by `/debug` as `First paint`, and must stay inside a 50 ms budget. `Ctrl+P` opens fuzzy command search; `Ctrl+F` and `Ctrl+R` open the flag and prompt pickers with the same filter-as-you-type behaviour. The renderer targets 60 frames per second while writing only changed terminal rows.

Sessions checkpoint to SQLite WAL every ten seconds at `~/.wrosecode/state.db`, while portable JSON session files remain available. Resume or fork work with:

```bash
wrosecode --session SESSION_ID
wrosecode --session SESSION_ID --fork
wrosecode --session SESSION_ID --export-session session.json
wrosecode --import-session session.json
```

Token counts, model latency percentiles, cache traffic, and costs come from provider usage. Configure prices in `pricing.toml` or `~/.wrosecode/pricing.toml`. Press `Ctrl+E` or run `/export` to write `metrics.json`, `metrics.csv`, and `latency.csv` (a latency histogram with one row per bucket: `bucket_upper_ms,count`). Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export the same counters to an OTLP HTTP collector.

Rate-limited provider replies (HTTP 429 / quota errors) are classified separately
from other failures, counted in the dashboard (`WARN 429s n`), and exported with
the rest of the metrics.

## Terminal compatibility

The TUI decides how much terminal control to take from the environment instead of
assuming a full xterm:

| Condition | Effect |
| --- | --- |
| `TERM` empty or `dumb` | No alternate screen, no mouse capture |
| VS Code / Cursor integrated terminal (`TERM_PROGRAM=vscode`, `VSCODE_PID`, `VSCODE_CWD`) | Mouse capture stays off so the terminal's native wheel scroll and text selection keep working |
| Zellij (`ZELLIJ` set) or `WROSECODE_FULL_REDRAW=1` | Every frame repaints fully (multiplexers composite their own grid and keep stale glyphs otherwise) |
| `WROSECODE_NO_ALT_SCREEN=1` | Run inline instead of switching to the alternate screen |

`mouse_capture` in `config.toml` overrides detection:

```toml
[ui]
mouse_capture = "auto"   # "auto" (default), "on", or "off"
alert_bell = true        #  before flag notifications
```

`auto` keeps native wheel scrolling inside VS Code and disables capture on dumb
terminals; `on` forces capture everywhere except `dumb`, `off` disables it
everywhere. The mouse is only used for the separator drag, so turning it off
costs nothing but that gesture.

## Advanced CTF workflow

The detector loads regexes from `config.toml`, checks plain output, ROT13, hexadecimal, and Base64 variants, and uses entropy to select encoded candidates. New flags pass through a checker before optional rate limited CTFd submission. `Ctrl+F` opens flag history and `/writeup` creates a Markdown evidence report.
While a picker is open, typing fuzzy-filters the list (`pdng` finds
`flag{padding...}`), `↑`/`↓` move, `Enter` copies the selected flag, and `Esc`
closes. `Ctrl+R` does the same for prompt history.

```bash
# CTFd operations
wrosecode ctfd list
wrosecode ctfd scoreboard
wrosecode ctfd download 42
wrosecode ctfd submit 42 'flag{answer}'
wrosecode ctfd verify 42 'flag{answer}'    # exit 0 verified, 2 rejected, 1 inconclusive

# Race the primary model against other configured models
wrosecode --provider vyce --model deepseek-v4.1 \
  --race openai:gpt-4o-mini --race ollama:qwen3 \
  'solve this challenge and verify the flag'
```

Project playbooks load from `skills/`, `.ctf/skills/`, and `~/.wrosecode/skills/`. Delegated tasks accept stable `task_id` values and can resume completed results. Thinking levels allocate runtime concurrency as follows: 0–3 uses one worker, 4–7 uses up to three, 8–12 uses up to five, and 13–20 allows the configured swarm limit.

The tool runtime includes MCP, Burp raw request import/export, optional Playwright screenshots, in-memory and Redis result caches, and an optional Qdrant writeup archive. Start the supporting services with `docker compose up -d`.

## Web and automation

```bash
# Local REST API and browser dashboard
wrosecode --provider vyce --model deepseek-v4.1 --web

# Headless automation with machine-readable completion data
wrosecode --headless --summary json 'analyze the supplied challenge'
```

The web server binds to `127.0.0.1:7878` by default. See `API.md` before exposing it through a reverse proxy.

## Shell sandbox

`[sandbox]` in `config.toml` decides where `shell` calls run:

| `engine` | Behaviour |
| --- | --- |
| `none` (default) | Run the command on the host, exactly like before. |
| `docker` | Run it inside a container. The project directory is bind-mounted at `/workspace`, so file edits made by other tools stay visible. |

```toml
[sandbox]
engine = "docker"
image = "wrosecode-sandbox:latest"
persistent = true      # one warm container reused across calls (no cold start)
name = "wrosecode-sandbox"
network = "bridge"
memory = "2g"
cpus = "2.0"
auto_build = false     # docker build -f Dockerfile.sandbox when the image is missing
```

Build the analysis image once:

```bash
docker build -f Dockerfile.sandbox -t wrosecode-sandbox:latest .
```

`engine = "docker"` never falls back silently: if the docker CLI, the daemon, or
the image is missing, the tool call fails with a message that tells you which of
the three to fix. `persistent = true` keeps one `sleep infinity` container alive
and `docker exec`s into it, so there is no per-call cold start.

```bash
/sandbox        # status: engine, container, image
/sandbox up     # warm the container now
/sandbox down   # stop and remove it
```

`auto_build = true` runs `docker build -f Dockerfile.sandbox -t <image> .` the
first time the image is missing, then falls back to `docker pull`.

## Run the agent in a container

Build the image that carries `wrosecode` itself plus the challenge toolchain:

```bash
docker build -t wrosecode-sandbox .
docker run --rm -it -v "$PWD:/workspace" \
  -e ANTHROPIC_API_KEY wrosecode-sandbox --permission yolo
```

With an interactive terminal, `wrosecode` opens a full-screen chat UI showing the conversation, tool activity, status, and a persistent input box. Anthropic text appears as it streams; the OpenAI-compatible backend displays its completed response. The UI redraws only changed lines, renders just the visible slice of a long transcript, and pins itself to the bottom while new output arrives (the header shows `· ↑N lines` when you have scrolled away). Type `/` at the start of the input to open the command palette. Keep typing to filter, use the arrow keys to select, press Enter to run, or Esc to close. `Tab` switches between build and read-only plan modes. Use `PageUp`/`PageDown` to scroll, `Up`/`Down` for prompt history, and `Ctrl+C` to exit. Messages typed while a turn is running are kept: they are queued and sent after the reply, and `Ctrl+C` during a turn cancels just that turn. One-shot calls and piped input keep plain text output. The UI displays permission requests and accepts `y` or `n`; `Ctrl+C` there denies the request and stays in the session.

The `delegate_task` tool can select a different `provider` and `model` for a child task. Supported provider names are `anthropic`, `openai-compat`, `ollama`, and `lm-studio`.

Attach an MCP stdio server for one run with `--mcp-bin PATH` and repeated `--mcp-arg ARG` flags. `/mcps add` saves additional servers in `~/.wrosecode/mcps.toml`; they reconnect at startup. Tools are exposed to the model as `mcp__SERVER__TOOL`.

Release installers are `install.sh` and `install.ps1`. Set `WROSECODE_RELEASE_BASE` to the URL of your published binary assets before using them; no release host is configured in this source tree.

## Configuration

`config.toml` in the project root is the single source of truth for the tunables;
`config.schema.json` describes it for editor validation. Every section and every
key is optional — a missing file, a missing section, or a typo falls back to the
built-in defaults rather than failing to start. CLI flags always win.

| Section | Purpose |
| --- | --- |
| `[ui]` | Theme, verbosity, alternate screen, `mouse_capture`, `alert_bell`, scroll step. |
| `[agent]` | Thinking level, worker ceiling, shell timeout, retries, permission tier, budget. |
| `[ctf]` | Flag regexes, clipboard copy, CTFd auto-submit. |
| `[cache]` | `memory` or `redis`, plus Redis and Qdrant URLs. |
| `[sandbox]` | Where `shell` calls run (`none` or `docker`) and the container settings. |

`cargo test` parses the shipped `config.toml` and asserts every key lands where
it should, so a section cannot silently stop being read again.

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Three layers run in that one command:

| Layer | Where | What it proves |
| --- | --- | --- |
| Unit | `src/**` modules | Flag detection corpus, metrics/histogram math, terminal profile rules, CTFd verdict classification, config parsing |
| CLI | `tests/cli.rs` | The argument surface stays documented: `--version`, `--help`, unknown flags, `ctfd verify --help` |
| End to end | `tests/e2e_headless.rs`, `tests/e2e_ctfd.rs` | A real `wrosecode` process against a local mock OpenAI-compatible server and a mock CTFd: tool round trips, `--summary json` usage numbers, `--until` exit codes, `ctfd verify` retries and exit codes |

The end-to-end tests bind `127.0.0.1` on an ephemeral port, point the child
process at a temporary `HOME`, and never touch the network or a real provider.

## Permissions

`--permission ask` prompts for mutating tool calls. `auto-safe` runs them automatically except risky shell commands. `yolo` removes normal prompts. A destructive shell-command blocklist prompts in every tier. `/plan` changes to read-only mode; `/build` returns to edit mode. The shell blocklist is a convenience guard, not an isolation boundary — for real isolation set `[sandbox] engine = "docker"` below, which moves every `shell` call into a container.

## Skills and memory

Skills are `SKILL.md` files with YAML frontmatter:

```yaml
---
name: my-skill
description: What it helps with
fork: true
---
Instructions for the agent.
```

Skills load from bundled `skills/`, project `skills/`, `~/.wrosecode/skills/`, and `--skills DIR`, with later paths overriding names. Use `#fact text` for project memory and `#!fact text` for personal memory. `/memory` lists saved facts. Matching facts are recalled automatically. Obvious secret patterns are redacted on save.

`/harness minimal|swe|claude` switches prompt style without clearing history. `wrosecode acp` starts a basic ACP stdio agent.

## Slash commands

`/help` shows the full command list, generated from the same registry as the palette.

| Area | Commands |
| --- | --- |
| Agents | `/agents`, `/build`, `/plan`, `/harness`, `/skills` |
| Providers | `/connect`, `/providers`, `/models`, `/model`, `/mcps` |
| Session | `/new`, `/sessions`, `/move`, `/editor`, `/memory`, `/greet`, `/debug`, `/stats`, `/verbosity`, `/theme`, `/export`, `/help`, `/exit`, `/quit` |
| Project | `/init`, `/diff`, `/review`, `/commit`, `/issues`, `/rmslop`, `/flags`, `/writeup`, `/sandbox` |

`/providers` opens a searchable provider screen with twelve built-in entries and their connection status. Enter configures or selects a provider; `a` adds, `e` edits, `d` deletes a custom provider, and `t` tests a connection. `/connect` opens the custom provider form directly. The API key field is masked. `/models` searches models across connected providers; `/model MODEL` switches the current provider's model.

Provider definitions are saved in `~/.wrosecode/providers.toml` without keys. Keys go to the OS keyring when available, otherwise to `~/.wrosecode/auth.json` with mode `0600`. Environment variables override stored keys. Custom provider keys can use `WROSECODE_NAME_API_KEY` (replace dashes in the name with underscores), or a saved `env:VARIABLE` reference. Custom providers support `openai_compat` and `anthropic` APIs; both support streamed replies and tool calls. Providers that need cloud-specific signing or URLs can use an OpenAI-compatible gateway as their base URL.

The same registry is available without the TUI:

```bash
wrosecode providers list
printf '%s\n' "$MY_API_KEY" | wrosecode providers add my-gateway \
  --base-url https://llm.example.com/v1 --stdin --model my-model
wrosecode providers set-key my-gateway --api-key env:MY_API_KEY
wrosecode providers edit my-gateway --model other-model
wrosecode providers test my-gateway
wrosecode --provider my-gateway 'Explain this project'
wrosecode providers remove my-gateway
```

Use `--stdin` or `--api-key env:VARIABLE` to keep keys out of shell history. You can add repeated `--header K=V` options for non-secret custom headers.

`/new` archives the current conversation to `~/.wrosecode/sessions/` and begins a new one. `/sessions` resumes a saved conversation, and `/move NAME` renames the current session. `/editor` opens `$EDITOR` (default `vi`) on a temporary prompt; saving and exiting submits it.

`/init` creates `.wrosecode/project.toml` and `WROSECODE.md` in the current repository without replacing existing files. `/diff` includes tracked and untracked changes. `/review` asks the model for findings without applying changes. `/commit` shows the diff, proposes a message, allows edits, and requires an explicit confirmation before staging and committing. `/issues` reads open GitHub or GitLab issues from the `origin` remote; private repositories can use `GITHUB_TOKEN` or `GITLAB_TOKEN`. `/rmslop` lists untracked scratch files and empty directories created by this session's file tool, then requires confirmation before deleting them.
