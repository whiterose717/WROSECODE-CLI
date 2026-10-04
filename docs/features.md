# Features

What WROSECODE ships today. Everything here is implemented and covered by
tests unless a line says otherwise.

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
  resume, provider/model routing), and `computer` (opt-in desktop control:
  `screenshot`/`click`/`type`/`key`/`scroll` via grim-family backends and
  xdotool, enabled by `--computer` or `[tools] computer = true`).
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
  `--import-session`, `/fork [name]` or `/fork <n> [name]` (headless:
  `--session ID --fork` / `--fork-at N`; `/history` prints the message
  numbers) to branch off a saved conversation from its end or from any
  earlier message, and `/tree` to render the fork forest with the active
  session marked. Sessions carry stable `ses_…` IDs (status line,
  `--session`/`-s`, `/sessions` with age labels); resuming restores history,
  provider, and model.
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
- Skill packages (pi-mono): `/skills install <git-url[#ref] | path>` (or
  headless `--install-skill`) copies a package into `~/.wrosecode/skills/`
  under hard ceilings — ≤64 files, ≤512KiB each, ≤2MiB total, two directory
  levels, no symlinks, every `SKILL.md` must parse — records its version
  (git short commit or content hash) in `~/.wrosecode/packages.json`;
  `/skills list` and `/skills uninstall <pkg>` manage them.
- Auto-commit (aider): after an edit turn whose checks pass, exactly the
  paths the agent touched are committed with a model-written subject derived
  from the staged diff (`[agent] auto_commit`, default on; `/commit` for
  manual commits) — unrelated dirty files in the tree stay out of the commit.
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
  only the running turn; at an idle prompt the first `Ctrl+C` arms a ~2s exit
  window and the second quits.
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
- Planner/worker split (goose): `[agent] planner = "provider/model"` (or a
  bare model on the main provider) names the model that runs the read-only
  `plan` mode — `/plan`, the Tab toggle, and `plan`-mode subagents think on
  it — while `build`/`general` keep the worker model. Built once per run,
  falls back to the worker when the planner profile is missing, shown in
  `/models`, `/plan`, and the status bar.
- Recipes (goose): YAML or Markdown workflow files with parameters and
  ordered prompt/command steps, discovered from `.wrosecode/recipes/` and
  `~/.wrosecode/recipes/`, run with `wrosecode recipe list` /
  `wrosecode recipe run <name> [--set KEY=VALUE …]` or the TUI's `/recipe`.
  Prompt steps stream through a normal turn (tools, permissions, steering),
  command steps run the author's own shell, `{{prev}}`/`{{steps}}` hand later
  steps the bounded output of earlier ones, and `--goal`/`--until`/
  `--summary json`/`--events` behave exactly as they do for a prompt run.

## Performance (Phase 7)

- Cold start: the splash is painted before provider setup, skill discovery,
  MCP reconnects, and session restore, so startup never waits on the network.
  The measured first-paint time is stamped into the frame, must stay inside
  the 50 ms budget (`splash::FIRST_PAINT_BUDGET_MS`, pinned by a unit test),
  and the PTY run in `tests/e2e_tui.rs` fails if the splash stops being the
  first frame or drifts past 250 ms on a debug CI build.
- Release profile: fat LTO, `codegen-units = 1`, `opt-level = 3`, stripped
  symbols, and `panic = "abort"` — the panic hook restores the terminal and
  writes the crash report *before* the abort, and nothing catches panics, so
  the hook plus abort composes. `./scripts/build-musl.sh` additionally
  produces a static `x86_64-unknown-linux-musl` build (static-pie, no glibc).
- One shared `reqwest` client with HTTP/2 (`http2` feature), connection
  pooling (`pool_max_idle_per_host = 8`), and sane connect/request timeouts
  feeds the provider, tools, and agent — no per-call TLS handshakes.
- SSE parsing buffers incrementally and drains complete `data:` events with a
  bounded scan (the CRLF pass only runs up to the LF hit), so a long stream
  stays linear instead of re-scanning the buffer on every event.
- Parallel tool execution: a batch of tool calls runs under a dynamic
  semaphore (1 / 3 / 5 slots by thinking level, capped by `max_parallel_tasks`).
- Result cache for `read_file`/`grep`/`glob` and matching read-only shell
  results, plus a loop detector: the fourth byte-identical `(name, input)`
  call in a row with no other tool in between fails locally with a "loop
  detected" message instead of burning another round-trip (streak resets per
  user query).
- Output shaping: ANSI/progress escapes stripped, consecutive duplicate
  lines collapsed, and long output split into head + tail under per-tool byte
  caps (`tools::truncate`).
- Stable cacheable prompt prefix: the system prompt is assembled as a
  byte-stable block (harness, overlay, AGENTS.md chain, pins, session
  constants, rules) followed by a boundary marker and a volatile tail (think
  level, recall, repo map, skill). The Anthropic adapter turns the boundary
  into a `cache_control` breakpoint so only the stable block is cached;
  OpenAI-compatible providers splice the marker back into one text so
  prefix-identity caches still hit.
- Benchmarks with regression budgets in `src/benchmarks.rs` (output shaping
  of 20k ANSI lines ≤ 100 ms, draining 1000 SSE events ≤ 50 ms, a cold
  repo-map walk ≤ 2 s). They run in release mode from `./scripts/check.sh`
  and CI (`cargo test --release benchmarks`) and fail the build when a hot
  path regresses; dev runs only print the timings. `scripts/benchmark.sh`
  additionally records test-suite latency and corpus size to
  `.ctf/reports/benchmark.json`.
- Deliberate adaptations: criterion was not pulled in for three numbers —
  the budgets are plain release tests (fewer moving parts, same CI gate);
  and speculatively dispatching the first read-only tool call while the
  response is still streaming was skipped, because batches already start
  the moment the response completes and dispatching mid-stream would race
  tool ordering, permissions, and the snapshot step for a marginal win.

## Hardening (Phase 8)

- Secrets never leave a surface they belong on: crash reports mask
  env vars whose names look like credentials (`KEY`/`TOKEN`/`SECRET`/… →
  `***`) and run argv, detail, and backtrace through a credential pattern
  set (AWS/GitHub/Slack/OpenAI keys, JWTs, `password=`/`Bearer `
  assignments). Configured provider-key values are registered when retrieved
  and removed even as bare strings. Tool titles/output, streamed text,
  dashboard process commands/tails, events NDJSON, session files/database
  payloads, and memory facts use the same filter; the live model context
  keeps full fidelity, while every persisted or displayed copy is redacted.
  Provider keys display as `••••last4`, are typed through a masked prompt, and are
  blocked from plain auth-header fields. Keys travel in HTTP headers, never
  in URLs, so provider error strings cannot echo them.
- Tool output, web pages, and file contents are untrusted *data*: they are
  only ever rendered into model context (repo map, memory recall, skill
  text, web results). Policy decisions — permission tiers, approval
  prompts, scope violations, plan-mode refusal — read exclusively from
  operator config, the user channel, and the structured tool-call
  arguments, so no injected instruction inside a fetched page or tool
  result can widen approvals or scope.
- Child processes are time-boxed and group-owned: `shell` commands run as
  their own process-group leaders; a timeout (or a cancelled turn, via the
  drop guard) SIGKILLs the whole group instead of just `sh`, so
  grandchildren cannot outlive the call. The dashboard Ctrl+C/Ctrl+K
  signal the group with a plain-pid fallback (and refuse pid 0, which
  would target the terminal's own group). stdout/stderr are tail-capped at
  256 KiB *while the command runs*, reader tasks get a one-second flush
  window before being aborted (a background daemon cannot hold the result
  open), and the docker engine runs an in-container `timeout -k 5` around
  the command — argv-safe (`$1` slot, no string interpolation) with a
  `command -v timeout` fallback for images without coreutils.
- Fuzzing with deterministic corpora (xorshift-seeded, identical on every
  run and in CI — a crash reproduces): the patch parser takes 400
  line-vocabulary patches through `paths`/`parse` plus 60 patches through
  `apply`, where an error must leave the target file byte-identical
  (atomicity holds under fuzz, not just in the hand-written cases); the
  SSE drain takes 400 separator-heavy byte streams and enforces a
  chunk-invariance property — every two-way split and a random three-way
  partition of each stream deliver exactly the events one whole feed
  does. Both sit in the normal test suite, so the standard gate fuzzes on
  every commit.

## UX and auth hardening (v0.3)

- The transcript owns the full terminal width: no always-on token or
  timeline panels. Token/latency/cost counters stay internal (status line,
  `/stats`, `live.json`, closing receipt). The subagent tree shows only
  in-flight work, newest last and capped, with finished subagents collapsed
  to one `✓ N subagents done` line. Tool command lines wrap instead of
  clipping; tool output keeps its capped previews with explicit markers.
- Idle `Ctrl+C` is two-stage (arm with a hint, second press within ~2s
  quits); cancelling a turn arms the same window. `/model` is gone —
  `/models` is the one model command, with an empty-state hint when nothing
  is configured.
- Every turn ends with a closing receipt: verification status, answer,
  proof, model/tools/wait split, a numbered step list, tokens, cache rate,
  cost, and savings (headless `--summary` prints the same steps).
- Startup paints the wordmark in progressive reveal stages between the init
  steps it was already doing — no added launch latency, first paint still
  inside the 50 ms budget.
- Any provider entry, built-ins included, can be removed; startup falls back
  to another configured provider instead of failing. OAuth sign-in
  (browser loopback with PKCE, or device code) stores tokens `0600` and
  refreshes before use; the client ID always comes from operator
  configuration.
- One tuned HTTP client shape (`provider::shared_client`) serves startup,
  provider tests, attach, CTFd, MCP, and OAuth. `--trace` logs per-step API,
  tool, and slow-render timings as NDJSON to `~/.wrosecode/trace.log`.

## Reference-project gap list

See `docs/ref-notes.md` for the per-project adoption record. Outstanding
items there: the deliberate
non-adopts (scheduled recipes, aider two-model mode and per-model edit
formats, client-side MCP requests) noted per project.
