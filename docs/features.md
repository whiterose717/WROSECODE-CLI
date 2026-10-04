# Features

What WROSECODE ships today. Gaps that Phase 6 must close are marked **TODO**;
everything not marked is implemented and covered by tests.

## Surfaces

| Surface | Entry | Notes |
| --- | --- | --- |
| Terminal dashboard | `wrosecode` | Differential renderer, splash, themes, PTY-tested in `tests/e2e_tui.rs` |
| Attach client | `wrosecode dashboard` | Re-renders the same grid from live.json → `/v1/status` → offline |
| Headless | `wrosecode --headless` | `--summary json` → `TaskComplete`, `--until`/`--goal` exit codes |
| Exec | `wrosecode exec --json` | One JSON document (`wrosecode/live-v1`) with answer, verification, usage |
| REST/web | `wrosecode --web` | `/`, `/health`, `/v1/status`, `/v1/chat`, `/v1/sessions/latest` |
| ACP | `wrosecode acp` | Editor-protocol server over the same `Agent` |

## Agent runtime

- Tools: `shell`, `read_file`, `write_file`, `edit_file` (exact-match replace,
  ambiguous matches rejected), `grep`, `glob`, `decode` (plain/rot13/hex/base64
  chains), `http`, `web_search`, `web_fetch`, `browser_capture`,
  `burp_import`/`burp_export`, `archive_writeup`, `search_writeups`,
  `update_plan`, `delegate_task` (parallel child agents with stable `task_id`,
  resume, provider/model routing).
- MCP: both transports the spec defines. stdio servers (a binary plus
  arguments) and streamable-HTTP endpoints (a `url` plus optional request
  headers) configured in `~/.wrosecode/mcps.toml` / `/mcps add`. The HTTP
  client posts JSON-RPC, captures the server's `Mcp-Session-Id`, and reads
  replies that arrive as one JSON document or as an SSE `data:` stream;
  the handshake and `tools/list`/`tools/call` are shared with stdio.
- Permission tiers `ask / auto-safe / yolo` (`Permission` in `src/config.rs`),
  destructive tools always prompt unless `yolo`.
- Sandbox engine `none | docker` (`[sandbox]`), an independent knob from the
  approval policy; timeouts and output caps in `run_with_timeout`.
- Instruction chain: `AGENTS.md` from the filesystem root down to the project
  plus `WROSECODE.md` (`/init`) is injected into every system prompt, each file
  capped at 8 KB and the chain at 32 KB (`project::instruction_chain`).
- Harness prompt styles `minimal | swe | claude` (`src/harness.rs`).
- Thinking levels `off|low|medium|high|max|auto` with per-provider control
  (`thinking.budget_tokens`, `reasoning_effort`) and a one-shot 400 fallback.
- Session persistence in SQLite plus the JSON files under
  `~/.wrosecode/sessions/` (both written by every run, so headless sessions
  show up in the TUI pickers), export/import via `--export-session` /
  `--import-session`, `/fork [name]` and `--session ID --fork` to branch off
  a saved conversation, and `/tree` to render the fork forest with the active
  session marked.
- Engagement coverage checklists (PentesterFlow): `.wrosecode/coverage.json`
  holds what has and has not been tested; the `coverage` tool (`list`, `add`,
  `done`, `undone`, unique-substring matching) lets the agent tick items off
  during a run, and `/coverage` shows the list, adds items, or opens an
  interactive picker to toggle them — persisted across sessions.
- Language-server diagnostics (`src/lsp.rs`, opencode parity): the result of
  `write_file`/`edit_file`/`apply_patch` carries what the file's language
  server says about the change — `LSP diagnostics (<server>) in <path>` with
  one-based `[severity] line:col message` lines — so a bad edit is corrected
  in the same turn. A dependency-free JSON-RPC client (Content-Length framing
  over tokio pipes) spawns servers lazily, one process per server command,
  warm-started when the file is read; `[lsp]` in `config.toml` sets
  `enabled`, `wait_ms`, and per-extension `commands` over the built-ins
  (rust-analyzer, typescript-language-server, pylsp, gopls, clangd). Missing
  binaries, hung handshakes, and protocol garbage all degrade to silence —
  never a failed edit.
- Metrics: counters, per-model totals, latency histogram, `metrics.json`/csv.
- Repo map (`src/repo_map.rs`): per-file symbols (syn for Rust, regex for
  other languages), mtime-cached, ranked by query relevance blended with a
  damped PageRank over the cross-file symbol-reference graph, emitted until a
  1024-token budget runs out (`/`-less prompting).
- Pinned file context (aider `/add` `/drop`): `/add <path>` injects the file
  into every system prompt (6k chars per file, 24k total), `/drop` unpins,
  `--add PATH` does it from a headless run; the list is saved with the
  session.
- Memory: project + personal markdown/jsonl with `remember`/relevance recall
  (`/memory`).

## CTF

- Detector with configurable regexes, entropy-ranked encoded candidates,
  CTFd list/scoreboard/download/submit/verify with shell exit codes, flag
  history picker, `/writeup`.
- **CTF autopilot** (`wrosecode ctf …`, TUI `/ctf`, auto-offer on a flag/CTF/
  challenge mention): scope allowlist printed at start, out-of-scope writes
  denied headless, triage probes, category hypothesis + stuck rotation,
  `ctf-notes.md` dead ends, `skills/ctf-*` playbooks, verification by
  re-derivation, candidate handling, budget stop report (`Tried`/`Learned`/
  `Next`), `writeups/<challenge>.md`, exit codes 0/2/1.

## Editing and git

- `/init`, `/diff`, `/review`, `/commit`, `/rmslop`, `/issues`, `/skills`.
- Lint/test auto-loop: `check_command` (`.wrosecode/project.toml`) runs after
  every successful `write_file`/`edit_file`/`apply_patch`, and a failed check
  triggers a repair pass.
- `/undo` + `/redo`: file-level snapshots under `.wrosecode/snapshots/` taken
  before each mutating batch (`src/snapshot.rs`); the replaced tree moves to
  the redo stack so `/redo` can reverse an `/undo`, a new edit invalidates the
  redo stack, and a turn that changed nothing discards its own snapshot.
## Agents and subagents

- `/plan` and `/build` agents, `@`-less subagent routing via `delegate_task`.
- Markdown user-defined agents and commands with YAML frontmatter:
  `.wrosecode/commands/*.md` expands `$ARGUMENTS` and runs as a turn (TUI and
  `--headless`), shows in `/help` and the palette; `.wrosecode/agents/*.md`
  contributes a system prompt, `mode:`, and `thinking:` selected through
  `/agents` or `--agent NAME`, and dropped by switching to a built-in.

## Orchestration

- Steering queue: keys land during a turn, `Enter` queues, `Ctrl+C` cancels
  only the running turn.
- Stuck detector: empty/repeated replies raise thinking, rotate the category
  hypothesis, and force a fresh-context attempt.
- Submission stream: `--events PATH` appends framed NDJSON (`start`, `step`,
  `text`, `result`) while a prompt/CTF run executes, so embedding clients get
  a live, machine-readable submission instead of the transcript; the `result`
  frame is written only after the live queue drains.
- Auto-compaction: the turn starts by summarizing the conversation once it
  passes `compact_at_chars` (180k characters by default, `WROSECODE_COMPACT_CHARS`
  overrides) and is at least six messages old; the summarizer sees every
  message capped at 6k characters, the history is replaced by the summary plus
  an acknowledgement, summarizer usage lands in the dashboard, and a failed
  summary leaves the history untouched. `/compact` forces a pass.
- Microagents (OpenHands): a skill whose frontmatter carries `keywords:`
  (YAML list or comma-separated string) triggers on a whole-word keyword hit
  in the prompt even when its name never appears, so a short knowledge
  snippet loads itself; the keyword gate scores below the skill's own name.
- Recipes (goose): YAML or Markdown workflow files with parameters and
  ordered prompt/command steps, discovered from `.wrosecode/recipes/` and
  `~/.wrosecode/recipes/`, run with `wrosecode recipe list` /
  `wrosecode recipe run <name> [--set KEY=VALUE …]` or the TUI's `/recipe`.
  Prompt steps stream through a normal turn (tools, permissions, steering),
  command steps run the author's own shell, `{{prev}}`/`{{steps}}` hand later
  steps the bounded output of earlier ones, and `--goal`/`--until`/
  `--summary json`/`--events` behave exactly as they do for a prompt run.

## Reference-project gap list

See `docs/ref-notes.md` for the per-project adoption record. Outstanding
items there: goose planner/worker model split; aider git auto-commit;
open-interpreter opt-in OS tools; pi-mono packages install + fork at an
arbitrary message; plus the deliberate non-adopts (scheduled recipes,
aider two-model mode and per-model edit formats, client-side MCP requests)
noted per project.
