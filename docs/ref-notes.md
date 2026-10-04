# Reference-project notes

What each reference project contributed to WROSECODE, and what was deliberately
left out. **Adopted** = implemented and covered by tests; **Partial** = the
idea exists in a weaker form; **Not applicable** = the surrounding design makes
it a poor fit.

## Codex

| Item | Status | Note |
| --- | --- | --- |
| Submission/event protocol | Adopted | `--events PATH` (or `-` for stdout) appends framed NDJSON: `start` (session, prompt), `step` frames mapped from `Progress` (tool begin/end with a 500-char output preview, thinking changes, plans, flag hits, usage), `text` frames for streamed tokens, and a final `result` (answer, verified, usage, model turns) written only after the live queue drains so it is always the last line. Covers prompt and CTF runs; `--summary json` / `exec --json` still emit the single `TaskComplete` object. `src/events.rs`, `tests/e2e_headless.rs::events_flag_writes_a_framed_submission_stream`. |
| `apply_patch` | Adopted | `tools::patch` parses `*** Begin Patch` documents (`Update`/`Add`/`Delete`, `*** Move to:`), applies every hunk with an exactly-once match, and validates the whole document before writing anything. Scope-checked per touched path, requires a prior read, blocked in plan mode, and feeds the check loop (`tests/e2e_headless.rs::apply_patch_adds_a_file_and_the_model_sees_the_result`). |
| Sandbox + approval as independent knobs | Adopted | `[sandbox] engine = none\|docker` is separate from `Permission::Ask\|AutoSafe\|Yolo`; both are read from config and overridable per run. |
| AGENTS.md chain | Adopted | `project::instruction_chain` walks `AGENTS.md` from the filesystem root down to the project, appends `WROSECODE.md`, caps each file at 8 KB and the chain at 32 KB, and injects it into every system prompt (`tests/e2e_headless.rs::agents_md_chain_reaches_the_system_prompt`). |
| Auto-compaction | Adopted | `Agent::compact` summarizes the history whenever `needs_compact()` fires — at the start of every `turn`, past `compact_at_chars` (180k characters, `WROSECODE_COMPACT_CHARS` overrides) and six messages — replacing it with the summary plus an acknowledgement; the summarizer sees each message capped at 6k characters, its usage is accounted, and a failed summary leaves the history untouched. `/compact` forces a pass. Covered by `agent::tests::compaction_replaces_the_history_with_a_summary` and `tests/e2e_headless.rs::a_large_resumed_history_is_compacted_before_the_turn`. |
| `exec --json` | Adopted | `wrosecode exec --json`, asserted in `tests/e2e_headless.rs`. |
| `resume` | Adopted | `--session ID` (+ `--fork`), `/sessions`, `latest`. |
| Profiles | Adopted | `settings::Settings` profiles with model, think, think_map. |

## opencode

| Item | Status | Note |
| --- | --- | --- |
| `build` / `plan` agents | Adopted | `/build`, `/plan` in the agent registry; plan is read-only by policy. |
| `@`-style subagents | Adopted | `delegate_task` with stable `task_id`, resume, per-child provider/model. |
| Markdown agents/commands with frontmatter | Adopted | `.wrosecode/commands/*.md` + `~/.wrosecode/commands/`: `description`/`category` frontmatter, body as prompt template with `$ARGUMENTS`, dispatched from the TUI (turn runs immediately) and from a `--headless` prompt that starts with the command name; entries also appear in `/help` and the palette. `.wrosecode/agents/*.md` + `~/.wrosecode/agents/`: `description`/`mode`/`thinking` frontmatter, body injected as `Active user agent (name):` into every system prompt (inherited by skill forks and `delegate_task` children), activated by `/agents` or `--agent NAME`; choosing a built-in mode clears it. `src/markdown.rs`, `Agent::set_user_agent`. |
| `/undo` `/redo` on snapshots | Adopted | `src/snapshot.rs` snapshots the exact paths a mutating batch touches under `.wrosecode/snapshots/undo/<id>/` *before* the edit, `/undo` restores it (deleting files the edit created) after keeping the replaced tree on the redo stack, `/redo` reverses that, a new edit clears the redo stack, and a turn that mutates nothing discards its own snapshot. Deliberately file-level rather than git commits: auto-committing a developer's working tree (or `reset --hard`) would sweep unrelated changes into our history, and snapshots work outside a git repo. Read-before-edit tracking and the read cache are invalidated on restore. |
| LSP diagnostics after edits | Not implemented | **TODO** — needs an LSP client; no dependency budget for one now, so feedback would have to come from `cargo check`/lint runs instead. |
| Client/server split | Adopted | `attach.rs` (live.json → `/v1/status` → offline) and `--web`. |
| Session list/share/export | Adopted | `/sessions`, `--export-session`, `--import-session`. |

## goose

| Item | Status | Note |
| --- | --- | --- |
| MCP as a first-class extension | Partial | stdio servers via `providers.toml`. **TODO** HTTP/SSE transports. |
| Recipes | Not implemented | **TODO** YAML/MD workflows with parameters + `wrosecode recipe run`. |
| Subagents | Adopted | `delegate_task`. |
| Scheduled/recurring recipes | Not implemented | Not applicable — a long-lived scheduler conflicts with the one-shot/TTY entry points; revisit only with a daemon mode. |
| Multi-model config (planner vs worker) | Partial | Per-child model routing exists; a global planner/worker split does not. |

## aider

| Item | Status | Note |
| --- | --- | --- |
| Repo map ranked by relevance | Adopted | `repo_map.rs` extracts symbols (syn for Rust, regex elsewhere), mtime-caches them, and ranks files by query relevance blended with a damped PageRank (0.85, 15 rounds) over the cross-file "file A mentions file B's symbols" graph. Output is capped at a 1024-token budget so a large repo cannot crowd the system prompt. |
| Git auto-commit + `/undo` | Partial | `/commit` proposes messages and confirms; auto-commit and `/undo` are missing. |
| Lint/test auto-loop after edits | Adopted | `config.check_command()` (`.wrosecode/project.toml`, written by `/init`) runs after every successful `write_file`/`edit_file`/`apply_patch` in `agent.rs`, feeding failures back as a repair turn. |
| `/add` `/drop` explicit context | Adopted | `/add <path>` pins an existing project file and injects its contents into every system prompt (6k chars per file, 24k total, truncated with a marker); `/drop <path|all>` unpins; listing is `/add` with no argument. Pins live on the agent, survive session save/resume, and the headless entry point takes the repeatable `--add PATH` flag. Skill-fork and delegate children start with an empty pin list. |
| Architect/editor two-model mode | Not implemented | Not applicable — `delegate_task` already lets a caller name a cheaper model per child; a global two-model toggle adds a second configuration axis for the same effect. |
| Multiple edit formats per model | Not implemented | Not applicable — one exact-match `edit_file` plus `apply_patch` covers the space without per-model format negotiation. |

## open-interpreter

| Item | Status | Note |
| --- | --- | --- |
| Run code in multiple languages | Adopted | `shell` runs python/bash/node/… through the same sandbox and approval path. |
| Profiles | Adopted | Harness styles + settings profiles. |
| `-y` auto-run | Adopted | `--permission yolo`. |
| OS/computer-control tools behind a flag | Not applicable | Only `browser_capture` ships, and it is opt-in via the tool list; a general OS-control surface has no safe approval story here yet. |

## pi-mono

| Item | Status | Note |
| --- | --- | --- |
| Extension/skill/package system | Partial | Skills load from `skills/`, `.ctf/skills/`, `~/.wrosecode/skills/`. **TODO** install from git/path with versions and sandboxing. |
| RPC/JSON mode | Adopted | `exec --json`, `--summary json`, `--web`. |
| Session tree / branching | Not implemented | **TODO** — `--fork` copies a session; there is no tree view. |
| Steering-message queue | Adopted | Queued input while a turn runs, `Ctrl+C` cancels the turn only. |
| Themes | Adopted | Six built-ins + user TOML palettes. |
| Differential TUI rendering | Adopted | Frame-diff renderer, PTY-tested. |

## OpenHands

| Item | Status | Note |
| --- | --- | --- |
| Containerised runtime | Adopted | `[sandbox] engine = "docker"`, persistent container, `/workspace` bind mount. |
| Stuck detector | Adopted | Empty/repeated replies raise thinking, rotate the category hypothesis, force a fresh context (CTF autopilot). |
| Context condenser | Adopted | The same mechanism as Codex auto-compaction (`Agent::compact` + `/compact`); the trigger here is transcript size rather than token-depth percentage, and the stuck detector still handles repeated-reply depth separately. |
| Keyword-triggered microagents | Not implemented | **TODO** — `skills::match_skill` picks the closest skill per prompt but has no keyword gating. |
| Delegation to child agents | Adopted | `delegate_task`. |
| Headless browser tool | Adopted | `browser_capture`. |
| Agent-server mode | Adopted | `--web` REST + `/v1/chat`. |

## PentesterFlow

| Item | Status | Note |
| --- | --- | --- |
| Permission tiers | Adopted | `ask / auto-safe / yolo`. |
| Persistent memory | Adopted | Project + personal markdown/jsonl, `/memory`, redaction on save. |
| Skill-fork for heavy playbooks | Adopted | `skills/ctf-*` playbooks plus `delegate_task` forks with their own context. |
| Coverage-tracking checklists | Not implemented | **TODO** — `/plan` tracks steps but there is no persisted coverage checklist. |
| Scope allowlists | Adopted | `Scope::for_target`, printed at start, enforced in `tools::execute` before dispatch. |

## clai (`pentoshi007/clai`)

Inspected 2026-10-04 (README + tree). Adopted:

1. **State-preserving compaction** — when `/compact` and auto-compaction land,
   keep the latest user request verbatim plus a work envelope (touched files,
   subagents, background jobs) instead of a lossy summary.
2. **Destructive-pattern blocking** — a `block` tier that refuses `rm -rf /`,
   fork bombs, and exfiltration-shaped commands regardless of the approval
   mode, layered on top of the existing ask/auto-safe/yolo tiers.
3. **One-shot natural language → shell** — a `--dry-run` style preview of the
   command the model would run, so a quick question does not need an approval
   round trip.

Deliberately skipped: provider/key sprawl and OAuth sign-in flows (we keep
`providers.toml` + env/keyring keys), persistent PTY REPL sessions, and the
`rtk` output rewriter (output shaping is Phase 7 work in our own code).
