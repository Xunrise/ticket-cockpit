# Cockpit: implementation plan

Implements `REQUIREMENTS.md`. The prototype (`tk.mjs`, `tui/`) is evidence only; the build starts clean.

Two deliverables:
1. **cockpit**: a Rust workspace in `~/Projects/ticket-cockpit` (empty repo, no remote).
2. **cogitator**: a feature branch on `Xunrise/cogitator` (PR), rebuilt from `proto/cockpit`.

## 1. cockpit

### Shape

One crate, one binary `cockpit`, subcommands:

| Command | What |
|---|---|
| `cockpit` | Launcher + TUI. Outside the cockpit's tmux: attach/create the `cockpit` session (starting the queue if missing). Inside: run the queue. |
| `cockpit daemon run` | HTTP API + SSE on 127.0.0.1:4747. What launchd starts. |
| `cockpit daemon status / start / stop / restart / logs` | Manage the background service from the CLI (`launchctl` underneath); `status` shows running/pid/port/db/uptime. |
| `cockpit onboard` | Seed a fresh install from how you already work (see Onboarding). |
| `cockpit hook` | Claude Code hook. Writes SQLite directly (no daemon needed), never fails. |
| `cockpit tool NAME '{json}'` | Any toolkit tool from the shell (used by Claude sessions). |
| `cockpit add/ready/show/claim/comment/close` | Ticket shortcuts for Claude sessions. |
| `cockpit install` | Idempotent setup: config dir, tmux config, launchd agent, global hooks (replaces the prototype's `tk.mjs` hook entries), prints what it changed. |

One binary keeps install and hook latency simple (native start ≈ ms instead of Node's ~50 ms per tool call).

### Modules (`src/`)

| Module | Responsibility |
|---|---|
| `db.rs` | Open SQLite (WAL, busy_timeout 2 s, so the hook and the daemon can write concurrently); schema + versioned migrations (`PRAGMA user_version`). |
| `paths.rs` | `~/Library/Application Support/cockpit/` (db, context files), `~/.config/cockpit/` (tmux conf). |
| `tickets.rs` | create (key per repo), update (editable fields, status events), claim, close (summary, branch, commits), ready, events, links. `repo_at(cwd)` via `git --git-common-dir`. |
| `projects.rs` | projects, Palaver link, services, overview (clusters, planned/doing/done, sessions). |
| `sessions.rs` | Hook state machine (SessionStart/UserPromptSubmit/Pre/PostToolUse/Notification/Stop/SessionEnd → working/needs/turn/ended), prompts log, trail, last message (transcript tail incl. AskUserQuestion), SessionStart context text. Mirrors state onto the cockpit pane (`@tk_state`). |
| `settings.rs` | Typed settings with defaults, descriptions and help texts; validation on set. |
| `tmux.rs` | Socket-aware adapter (`Server::Cockpit` / `Server::Default`, always explicit `-L`), pane refs as `cockpit:%N` / `%N`, list panes, capture, send keys, options, windows, join-pane, respawn. All tmux calls go through here. |
| `panes.rs` | Ticket windows (`cockpit_window`), named service panes, layout (strip below Claude), pane.up from project services, env cleanup (`CLAUDE*` stripped, `PWD` set). |
| `prompt.rs` | Screen parsing for safe sending: input box between the last two rules, ghost text (dim SGR) vs draft, question menu, vim insert state; `send_text` with the check-before/check-after rules. Pure functions + fixtures. |
| `palaver.rs` | Minimal MCP stdio client (initialize → tools/call), meeting → Markdown, coach baseline file. |
| `summary.rs` | Summary (attention, working, ready, done today, counts, projects, focus, calls) and change detection for SSE. |
| `tools.rs` | Tool registry: name, description, params, handler `fn(&Ctx, Value) -> Result<Value>`. One table used by HTTP and CLI (and MCP later). |
| `connections.rs` | Connection status + help texts. |
| `http.rs` | axum: Host check, `x-tk` guard, `/api/summary`, `/api/stream`, `/api/tools`, `/api/tools/:name`, `/api/state`, `/api/pane/:sid` and `/send`. |
| `tui/` | `app.rs` (state, tick, key routing), `queue.rs`, `projects.rs`, `settings.rs`, `travel.rs` (where the queue sits; follow the user's window; home slot content), `bindings.rs` (queue key, Esc menu, `@tk_confirm_esc` sync), `widgets.rs` (hint chips, pickers, fuzzy). Talks to the daemon over HTTP only. |
| `assets/` | `cockpit.tmux.conf`, launchd plist template (embedded with `include_str!`). |

Error handling: `anyhow` at the edges (CLI, HTTP handlers return `{error}` with 400), typed errors where callers branch (e.g. `SendRefused::{Question, Draft, NotTaken}`). The hook catches everything and exits 0.

Dependencies (all common, nothing exotic): `rusqlite` (bundled), `serde`/`serde_json`, `axum` + `tokio` (daemon only), `ratatui` (+ its crossterm), `ureq` (TUI → daemon), `anyhow`. Time via `time` or plain epoch math; no ORM.

### What changes vs. the prototype

- Daemon is a launchd agent (`io.cockpit.daemon`, KeepAlive) instead of a window inside the cockpit's tmux, so the cogitator and Waypoint can rely on it without the queue running. It still addresses both tmux servers explicitly.
- The hook writes the database directly instead of going through Node.
- Ticket windows live in the `cockpit` session from the start (no per-repo sessions).
- Settings and connection help texts move from inline strings into `settings.rs` / `connections.rs` tables.
- The prototype's web board (`index.html`) is dropped; the HTTP API stays for other frontends.

### Tests (pin the confirmed behaviour)

- `tickets`: keys per repo, ready ordering + blockers, claim/close records branch and commits (temp git repo), status events.
- `sessions`: each hook event → state; AskUserQuestion → needs with questions as message; Stop fallback to transcript; SessionStart context includes the project and cockpit-tmux paragraphs only when they apply; `TK_ISSUE` claims.
- `summary`: focus (threads in 30 min, streak with a 30-minute gap, forgotten > 30 min, orphaned), done today, ordering.
- `prompt`: fixtures captured from real Claude panes: empty prompt, ghost suggestion, real draft, open question, vim normal vs insert → correct decision; send reports "not taken" when the draft remains.
- `settings`: defaults, type validation, reset.
- `tools`: `ticket.for_change` join order (session → repo+branch → key in branch), `ticket.add` repo fallback chain, overview clustering (unsorted last).
- `tmux` + `panes` integration tests on a private socket (`-L cockpit-test-<pid>`): window per ticket, pane.run creates/respawns by name, layout puts services below Claude, travel (join-pane) and following the user's window, `@tk_state` mirroring. Skipped when tmux is missing.
- `http`: Host check and `x-tk` guard (403s), SSE emits on change.
- `tui`: ratatui `TestBackend` snapshots: queue rows with hint chips, projects list → project view, settings tab with the right-side text, tab cycling keeps the open project.
- `palaver`: MCP client against a stub process that speaks the same JSON-RPC lines.

Run with `cargo test`; `cargo clippy -- -D warnings` and `cargo fmt --check` as the lint gate.

### Steps (atomic commits on `feat/cockpit`)

1. Scaffold: workspace, `db.rs` + migrations, `paths.rs`, CI-less `cargo test` passing.
2. Tickets + projects + CLI shortcuts.
3. Hook + sessions state machine (+ tests); `cockpit install` for hooks only (replacing the proto entries).
4. tmux adapter + panes (+ integration tests).
5. Toolkit + HTTP daemon + summary/SSE + launchd install.
6. `prompt.rs` safe sending.
7. Palaver client + meeting linking + auto-link session.
8. TUI: queue tab + travel + bindings + hints.
9. TUI: projects tab + housekeeping.
10. TUI: settings tab + connections + help texts.
11. `cockpit install` complete (tmux conf, launchd, hooks), `cockpit daemon status/start/stop/restart/logs`, README with setup and keys.
12. Validate against the converted prototype data, then `cockpit onboard`.
13. Switch over: point the global hooks at `cockpit hook`, stop the prototype daemon, start the launchd agent.

## 2. cogitator (branch `feat/cockpit` off `main`)

Port the three prototype changes cleanly:

- `spirit/daemon.py`:
  - `COCKPIT` URL from env (default `http://127.0.0.1:4747/api/summary`).
  - `read_cockpit` with a 3 s timeout and 15 s hold of the last summary.
  - `sync_cockpit` mapping cockpit sessions onto the existing session shape, with done/needs transitions and one feed "turn" per completed turn.
  - `work_context` appended to every Ollama prompt; `coach` nudges with the cooldowns from the requirements.
  - The after-call tip from `calls`.
  - Persona lines allowing ticket keys and care nudges.
  - `cockpit` in `/state.json`.
  - The old hook-log path stays as the fallback.
- `home/.config/sketchybar/plugins/realm.py`: `realm.attn0..4` items (needs/turn only, longest first), the `cockpit` click action (`@tk_select` + Ghostty with `cockpit`), fallback to the tmux/loose slots without the cockpit.
- `spirit/wallpaper/arcane.html`: `drawCockpit` (project rings, session bodies, ready moons, wobble, dust) and `drawDone`. Session constellations: one star per turn, closed when the session ends (cockpit mode), old behaviour otherwise.
- Tests in `spirit/tests/test_spirit.py`:
  - `sync_cockpit`: transitions, one star per turn, sessions removed when gone.
  - `read_cockpit`: tolerance window.
  - `coach`: each nudge's trigger and cooldown.
  - `work_context`: text.
  - `realm.slots`: with and without the cockpit.
- Docs: a short section in the README on the cockpit integration.
- Push and open a PR. After merging, run `install.sh` from `main` so launchd, sketchybar and Übersicht point back at the main checkout; delete `cogitator-proto-cockpit`.

### Data and onboarding

- The real install uses its own database in `~/Library/Application Support/cockpit/` and starts empty; `cargo run` uses the same unless `COCKPIT_DB` points elsewhere.
- Validation: a copy of the prototype's `data.db` is converted into a test database (`COCKPIT_DB=…`) and the new build is checked against it (queue, projects, settings, Palaver links). It is never imported into the real database.
- `cockpit onboard` (re-runnable, asks before writing):
  1. Repos: scans `~/.claude/projects/*` transcripts of the last 60 days for working directories, resolves them to repos (worktrees → main repo), lists them with session counts.
  2. Projects: proposes one project per repo (or groups, e.g. the cellworks and wishlang demo folders), lets you rename/merge/skip, and offers to link each to a Palaver project.
  3. Open threads: starts a Claude session in the cockpit (`onboarding` window) that reads the recent transcripts per project and proposes tickets for unfinished work (open questions, "later", TODOs, stories mentioned), filed through `ticket.add` after you confirm; plus default service panes when it sees dev servers being started.
  4. Live sessions: currently running Claude sessions show up by themselves through the hooks.

## Cleanup (Phase 4 step 1)

- Stop the prototype daemon window and the `cogitator` proto daemon session in the cockpit's tmux.
- Delete `~/Projects/ticket-cockpit-proto` (after the optional data import) and the `proto/cockpit` cogitator worktree + branch.

## Decisions (2026-11-18)

1. Web board dropped, API kept.
2. Prototype data only for validation; the real install starts from `cockpit onboard`.
3. Daemon is a background service (launchd) with `status/start/stop/restart/logs` in the CLI.
4. Name: open (see conversation).
