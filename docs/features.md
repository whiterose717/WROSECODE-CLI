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
- MCP: stdio servers configured in `providers.toml`. **TODO** HTTP/SSE MCP.
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
- Session persistence in SQLite (plus JSON export/import via
  `--export-session` / `--import-session`).
- Metrics: counters, per-model totals, latency histogram, `metrics.json`/csv.
- Repo map (`src/repo_map.rs`): per-file symbols (syn for Rust, regex for
  other languages), mtime-cached, keyword-ranked for `/`-less prompting.
  **TODO** PageRank over references and a token budget.
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
- **TODO** `apply_patch`, `/undo` + `/redo` on git snapshots, lint/test
  auto-loop after edits, explicit `/add` + `/drop` context.

## Agents and subagents

- `/plan` and `/build` agents, `@`-less subagent routing via `delegate_task`.
- **TODO** markdown user-defined agents and commands
  (`.wrosecode/agents/*.md`, `.wrosecode/commands/*.md`) with frontmatter.

## Orchestration

- Steering queue: keys land during a turn, `Enter` queues, `Ctrl+C` cancels
  only the running turn.
- Stuck detector: empty/repeated replies raise thinking, rotate the category
  hypothesis, and force a fresh-context attempt.
- **TODO** context compaction, microagents (keyword-triggered knowledge
  snippets), recipes (`wrosecode recipe run`), scheduled/recurring work.

## Reference-project gap list

See `docs/ref-notes.md` for the per-project adoption record. Outstanding
items there: Codex submission/event protocol, `apply_patch`, auto-compaction; opencode markdown agents/commands, `/undo` `/redo`, LSP
feedback, session tree; goose recipes + HTTP MCP + planner/worker models;
aider repo-map ranking, auto-commit loop, `/add` `/drop`, two-model mode,
edit formats; open-interpreter multi-language runner and opt-in OS tools;
pi-mono packages + session branching; OpenHands condenser + microagents;
PentesterFlow coverage checklists.
