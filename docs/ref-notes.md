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
| LSP diagnostics after edits | Adopted | `src/lsp.rs`: a dependency-free JSON-RPC client (Content-Length framing over tokio pipes) that starts a language server lazily — one process per server command, warm-started when the file is read — finishes `initialize`, re-opens the touched file on every edit, waits `[lsp].wait_ms` for `publishDiagnostics`, and appends `LSP diagnostics (<server>) in <path>` with one-based `[severity] line:col message` lines to the `write_file`/`edit_file`/`apply_patch` result. `[lsp]` in config.toml: `enabled`, `wait_ms`, per-extension `commands` (an empty list removes a built-in; defaults: rust-analyzer, typescript-language-server, pylsp, gopls, clangd). Every failure mode — missing binary, hung handshake, protocol garbage — degrades to no diagnostics rather than a failed edit. |
| Client/server split | Adopted | `attach.rs` (live.json → `/v1/status` → offline) and `--web`. |
| Session list/share/export | Adopted | `/sessions`, `--export-session`, `--import-session`. |

## goose

| Item | Status | Note |
| --- | --- | --- |
| MCP as a first-class extension | Adopted | Both transports: stdio servers and streamable-HTTP endpoints in `~/.wrosecode/mcps.toml` (`McpServerDef { url, headers }`, `src/settings.rs`); `Mcp::connect_def` picks the wire (`src/tools/mcp.rs`). HTTP posts JSON-RPC with `Accept: application/json, text/event-stream`, replays `Mcp-Session-Id`, and decodes either a JSON body or SSE `data:` events; a wrong-key/non-2xx reply fails the connect. `stdio` handshake (initialize → initialized → tools/list, 20s each) is shared so the two paths cannot drift. `/mcps add` asks for an endpoint URL first (blank = binary). Tests: axum mock server (JSON + SSE + session pin) in `src/tools/mcp.rs`, mcps.toml parse/round-trip in `src/settings.rs`. Server-initiated requests (the server calling back into the client) are out of scope. |
| Recipes | Adopted | `src/recipe.rs`: YAML (or Markdown with frontmatter) files in `.wrosecode/recipes/` and `~/.wrosecode/recipes/` carrying `params` (names, or a map of `description`/`default`/`required`) and ordered `prompt:`/`command:` steps. `wrosecode recipe list`, `wrosecode recipe run <name> --set KEY=VALUE …`, and the TUI's `/recipe` (picker, then a question per required parameter) all resolve defaults, reject unknown keys, substitute `{{key}}`, and pass `{{prev}}`/`{{steps}}` (12k bounded) between steps. Prompt steps are ordinary turns; command steps run the author's own shell through `run_with_timeout`. `--summary json`, `--events`, `--goal`/`--until` share the prompt path's `print_outcome`. `tests/e2e_headless.rs::a_recipe_runs_each_step_in_order_with_parameters`. |
| Subagents | Adopted | `delegate_task`. |
| Scheduled/recurring recipes | Not implemented | Not applicable — a long-lived scheduler conflicts with the one-shot/TTY entry points; revisit only with a daemon mode. |
| Multi-model config (planner vs worker) | Adopted | `[agent] planner = "provider/model"` (or a bare model, keeping the worker's provider) — `AgentRuntimeConfig::planner_split` (`src/config.rs`). The read-only `plan` mode builds that provider once per run and every outer loop iteration selects it (`plan_phase_uses_planner` + `selected_provider` in `src/agent.rs`); `build`/`general` and `delegate_task` children keep the worker. A missing/broken planner profile degrades to the worker rather than failing the turn. Surfaced in `/models`, `/plan`, the Tab-toggle status, and the shipped `config.toml` (commented out). |

## aider

| Item | Status | Note |
| --- | --- | --- |
| Repo map ranked by relevance | Adopted | `repo_map.rs` extracts symbols (syn for Rust, regex elsewhere), mtime-caches them, and ranks files by query relevance blended with a damped PageRank (0.85, 15 rounds) over the cross-file "file A mentions file B's symbols" graph. Output is capped at a 1024-token budget so a large repo cannot crowd the system prompt. |
| Git auto-commit + `/undo` | Partial | `/commit` proposes messages and confirms; auto-commit is deliberately left to the user (the snapshot-based `/undo` `/redo` under opencode covers editing mistakes without sweeping a working tree into git history). |
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
| Session tree / branching | Partial | `/fork [name]` and `--session ID --fork` branch a session (recording `parent`), `/tree` renders the forest from the JSON files with the active session marked, and headless runs now write those files too so every branch is listed. Forks happen at the current end of the conversation; branching back to an arbitrary earlier message is still missing. |
| Steering-message queue | Adopted | Queued input while a turn runs, `Ctrl+C` cancels the turn only. |
| Themes | Adopted | Six built-ins + user TOML palettes. |
| Differential TUI rendering | Adopted | Frame-diff renderer, PTY-tested. |

## OpenHands

| Item | Status | Note |
| --- | --- | --- |
| Containerised runtime | Adopted | `[sandbox] engine = "docker"`, persistent container, `/workspace` bind mount. |
| Stuck detector | Adopted | Empty/repeated replies raise thinking, rotate the category hypothesis, force a fresh context (CTF autopilot). |
| Context condenser | Adopted | The same mechanism as Codex auto-compaction (`Agent::compact` + `/compact`); the trigger here is transcript size rather than token-depth percentage, and the stuck detector still handles repeated-reply depth separately. |
| Keyword-triggered microagents | Adopted | Skill frontmatter takes `keywords:` (YAML list or comma-separated string, lowercased and deduplicated at parse time). `skills::match_skill` scores an explicit whole-word keyword hit at 60 + name-word hits — above fuzzy name matching, below the skill's own name — and picks the highest score, so a short snippet injects itself the moment one of its words is mentioned. |
| Delegation to child agents | Adopted | `delegate_task`. |
| Headless browser tool | Adopted | `browser_capture`. |
| Agent-server mode | Adopted | `--web` REST + `/v1/chat`. |

## PentesterFlow

| Item | Status | Note |
| --- | --- | --- |
| Permission tiers | Adopted | `ask / auto-safe / yolo`. |
| Persistent memory | Adopted | Project + personal markdown/jsonl, `/memory`, redaction on save. |
| Skill-fork for heavy playbooks | Adopted | `skills/ctf-*` playbooks plus `delegate_task` forks with their own context. |
| Coverage-tracking checklists | Adopted | `.wrosecode/coverage.json` holds the durable checklist (separate from `/plan`, which stays transient). The `coverage` tool — `list`/`add`/`done`/`undone` with exact-then-unique-substring matching — lets the model check items off mid-run, and `/coverage` renders the list, appends items from the command line, or opens a picker loop that toggles entries on Enter until Esc; both paths write the same file. |
| Scope allowlists | Adopted | `Scope::for_target`, printed at start, enforced in `tools::execute` before dispatch. |

## clai (`pentoshi007/clai`)

Inspected 2026-10-04 (README + tree).

| Item | Status | Note |
| --- | --- | --- |
| State-preserving compaction | Adopted | `/compact` and auto-compaction pin the newest user request verbatim into the wrapper after the summary — it rides through the cut no matter what the summarizer drops — while `COMPACT_SYSTEM` is told to keep goals, constraints, touched files and decisions (a prompt-driven work envelope rather than a mechanically assembled one). |
| Destructive-pattern blocking | Adopted | `shell::destructive()` matches `rm -rf`/`rm -fr`, `mkfs`, `dd if=`, fork bombs (`:(){`), `chmod -r 777 /` and raw writes to block devices, and `tools::execute` raises it above every approval mode: it prompts even in `yolo`, has no auto-approve path, and a run without a terminal is refused, so it never executes unattended. |
| One-shot natural language → shell | Not implemented | There is no one-shot mode and no `--dry-run` preview: `--prompt` runs the full agent loop, and `--plan` is the read-only substitute for "just show me what you would do". |

Deliberately skipped: provider/key sprawl and OAuth sign-in flows (we keep
`providers.toml` + env/keyring keys), persistent PTY REPL sessions, and the
`rtk` output rewriter (output shaping is Phase 7 work in our own code).
