# Cockpit: requirements

Derived from the prototype in `ticket-cockpit-proto/` (Node daemon `tk.mjs`, Rust TUI `tui/`) and the cogitator prototype on branch `proto/cockpit`, as confirmed in conversation on 2026-11-18.

## Scope and constraints

- Single user, local only. No accounts, no remote sync, no PR-centric flow.
- The cockpit is the hub for the user's work: tickets, projects, Claude sessions, terminals (tmux), meetings (Palaver), the desktop layer (cogitator), and review (Waypoint).
- A daemon owns the state and exposes it to several frontends: the TUI (primary), cogitator, Waypoint, and Claude sessions (CLI now, MCP later).
- All state lives in one local SQLite file.
- The HTTP API binds to 127.0.0.1 only. It rejects requests whose Host is not localhost/127.0.0.1. Every non-GET request needs the header `x-tk: 1`, because the API can type into Claude sessions.
- Claude Code sessions are tracked through global hooks in `~/.claude/settings.json`. A hook never blocks or breaks a session: errors are swallowed, timeout 5 s, it must be fast because it runs on every tool call.

## Tickets

- Fields: key, repo, title, body, status (`backlog | todo | doing | done | canceled`), priority 1–4, project, cluster, blocked_by, claiming session, rank, created, updated.
- Key = first three letters of the repo name in upper case + running number (`DEM-7`).
- A ticket's repo is resolved through `git rev-parse --git-common-dir`, so worktrees count as their main repo. Repos and their paths are remembered.
- "Ready" = `todo` with no blocker, or a blocker that is `done`, ordered by rank.
- Every ticket has an event log: created, status changes, claims, comments, linked context, close summary.
- Closing a ticket records the summary, the branch, and the commits between the session's start HEAD and HEAD.
- Links: a ticket links to context items: Palaver meetings (with the saved notes/transcript file and an optional "why") and Waypoint cases.

## Projects

- A project groups tickets across repos. It is created when a ticket is first assigned to it.
- A project can be linked to one Palaver project.
- A project can have default service panes: a list of `{name, cmd, cwd?}`.
- A project overview shows: planned tickets (todo/backlog) grouped by cluster with unclustered last; tickets in progress with their session's state; recently done tickets; live sessions; known clusters.

## Claude sessions (hooks)

- States: `working` (prompt submitted or tool running), `needs` (permission prompt or AskUserQuestion open), `turn` (Claude stopped, waiting for the user), `ended` (SessionEnd, or the session's tmux pane is gone).
- Recorded per session: repo, cwd, tmux pane (with which tmux server it lives on), transcript path, HEAD at start, last prompt, last assistant message (falling back to the transcript; an open AskUserQuestion's questions count as the message), last 5 tool calls, every prompt's timestamp.
- SessionStart:
  - claims the ticket named in `TK_ISSUE`;
  - injects context: the session id and how to use the cockpit's tools;
  - if the ticket has a project: tells Claude to file side ideas and ambitions that don't belong in this session as new tickets in that project (one line to the user about it), and how to read the project overview;
  - if the session runs in the cockpit's tmux: tells Claude it runs next to the user's queue, must never run servers/watchers as background tasks, and must use named panes instead (`pane.run`, `pane.up`, `pane.logs`, `pane.stop`, `project.set_services`).
- The session's state is mirrored onto its cockpit tmux pane as the pane option `@tk_state`.

## The cockpit's tmux

- The cockpit runs its own tmux server (socket `cockpit`) with its own config; the user's `~/.tmux.conf` and default tmux server are never touched.
  - Config: mouse on, no status bar, true colour, no escape delay, 50k scrollback, base-index 1, heavy pane borders, extended keys enabled for Ghostty so Shift+Esc is distinguishable.
  - Picker (`prefix w` / `prefix s`): only the `cockpit` session; its home window `cockpit` pinned at index 0 and highlighted (`◆ cockpit`, bold blue); background sessions (daemon, cogitator) hidden.
- Running `cockpit` from any terminal attaches to (or creates) that tmux session; if the session exists without a queue, the queue is started first. Inside the cockpit's tmux it runs directly.
- Every ticket has a window named after its key in the `cockpit` session, holding the Claude pane plus named service panes. Searches and housekeeping sessions get windows too (`DEM-7-ctx`, `Demo-housekeeping`).
- Starting a session for a ticket: new window in the repo's main path, `TK_ISSUE` set, inherited `CLAUDE*` variables stripped and `PWD` set (otherwise the transcript is not saved and the session may start in the wrong directory), initial prompt "Work on ticket KEY: title…". The window keeps a shell after Claude exits. The session decides about worktrees.
- Service panes: named (`@tk_name`), laid out as a strip below the Claude pane (30% of the height), side by side; restarting a pane by name respawns it; a pane shows the exit code when its command ends and keeps a shell.
- Sessions on the user's default tmux server or in the desktop app are visible and can be replied to, but can't be shown inside the cockpit (tmux can't move panes between servers).

## The queue (TUI)

- Three tabs: **queue**, **projects**, **settings**. `Tab` cycles forward, `Shift+Tab` back; the opened project is remembered across tab switches. `,` jumps to settings.
- The queue pane travels: it joins the window of whatever is selected, on the left at the configured width (default 38%), full height. The right side shows that window's real panes (Claude + services). With nothing tmux-backed selected it sits in the home window, whose right pane shows the selection's details.
- If the user switches windows themselves (picker, mouse), the queue follows; on the home window it stays until another row is picked.
- The queue key (default Shift+Esc) jumps back to the queue from any pane; `Enter` on a session moves the cursor into its Claude pane (no zoom).
- `q` detaches the terminal(s) showing the queue; the cockpit keeps running. `Q` stops the queue itself.
- The tmux window of the queue is renamed `cockpit` and moved to index 0 on start.

### Queue tab

- Sections NEEDS YOU, YOUR TURN (longest wait first), WORKING (most recent first), ENDED (doing tickets whose window exists but whose Claude session is not live; chips `[s] restart session  [⏎] open window`), READY (rank order).
- Rows: ticket key + title (sessions without a ticket: their cockpit window name, else repo · branch), time in state, then repo · branch · tmux location · ◆ project.
- The selected row shows its hotkeys underneath (e.g. `[⏎] attach [1-4] pick [a] accept [r] reply [x] interrupt`).
- The bottom of the queue shows the selected session's last message (or recent tool calls while working), its project, Palaver link and linked context.
- Keys: `j/k`, `⏎` attach, `r` reply (plain input line), `1-4` pick an option, `a` accept (Enter), `x` interrupt (sends Esc), quick replies (configurable, default `c` continue, `y` yes, `t` try again, `m` merge it, `d` start the dev server), `s` start a session for a ready ticket, `p` assign a project (fuzzy picker, typing a new name offers "+ new project" as the last entry), `l` link meeting context.
- The footer shows only `⇥ tabs · ? all keys · q leave`; `?` toggles the full key list.

### Hints on the session's pane

- The pane border of a ticket window is at the bottom, directly under the Claude pane.
- While the queue is focused it shows the ticket in bold and the context's keys in bold blue: waiting on an answer `1-4 pick · a accept · x interrupt · <quick replies> · r reply · ⏎ attach`; your turn: quick replies, `r reply`, `⏎ attach`; working: `x interrupt · ⏎ attach`.
- While attached it shows the ticket and `⇧esc back to queue`. Service panes show their name, the queue pane shows `queue`.

### Sending to a session (reply, quick replies)

- The cockpit checks the screen first and refuses, with a message, when Claude shows a question ("answer with 1-4 or a, or attach") or when the prompt already holds the user's own text (never append to a draft).
- Claude's dim suggested prompt (ghost text) is not a draft.
- With vim editor mode (`editorMode: "vim"` in `~/.claude/settings.json`) and the prompt not in insert mode, it enters insert mode first.
- After Enter it checks again and reports when the text is still sitting in the prompt. Only real success reports "sent".

### Esc on a working Claude

- Esc behaves normally everywhere (menus, vim mode, Esc-Esc).
- In a Claude pane whose state is `working`, and with confirmation enabled, Esc opens a menu "Claude is working": `i` interrupt the running task, `k` keep it running, `n` interrupt and never ask again (turns the setting off).

### Projects tab

- First a list of projects (planned / in progress / done counts, Palaver link); `⏎` opens one. `Esc` goes back to the list; `o` opens a fuzzy project picker.
- Inside a project: PLANNED grouped by cluster (unsorted last), IN PROGRESS with session state, DONE.
- Keys: `/` filter (fuzzy, filters locally while typing), `n` new task, `g` cluster (existing clusters first, "+ new cluster" last), `H` let Claude housekeep, `s` start a session, `⏎` open the ticket's live session.
- The right side follows the selected ticket: its live cockpit session's window, or (no live cockpit session) the ticket itself: key, title, status, priority, project, cluster, body, linked context, sessions.
- Housekeeping (`H`) starts a Claude session in a `<Project>-housekeeping` window that reads the project overview and, without implementing anything: merges duplicates (cancels the duplicate, folds its details in), clusters, sharpens titles, sets obvious priorities, and asks before dropping anything.

### Settings tab

- Settings (Enter toggles a checkbox or edits a value, `r` resets to default):
  - queue key (default `S-Escape`)
  - confirm interrupt (default on)
  - queue width (default 38 %)
  - start services on dispatch (default off)
  - paste meeting context (default on)
  - quick replies (`key=text; …`)
- Connections (status only): Claude Code hooks, Claude MCP (planned), Cogitator, Ollama, Palaver, Waypoint, Jira (placeholder), GitHub Issues (placeholder). Status values ok / partial / off / planned.
- The right side explains the selected entry: description, value and default, a longer explanation; for queue width a drawing of the split at the current value; for each connection what it is, what it provides, how it's wired, and what "off" or "partial" means. Text wraps to the pane and keeps its indent.

## Palaver

- Accessed through Palaver's own MCP server (`palaver mcp`, stdio JSON-RPC), not its internals.
- `l` on a ticket: if the ticket has no project → project picker; if the project isn't linked to Palaver → Palaver project picker; then a fuzzy picker of that project's meetings (date, title, length, summary, ✓ already linked).
  - `⏎` links the meeting: notes and transcript saved as Markdown, link stored, and (setting on) a reference line pasted into the ticket's live Claude prompt without submitting.
  - `ctrl-a` starts a Claude session (`KEY-ctx` window) that searches the project's meetings through Palaver and links the relevant ones itself via the cockpit's tools.
- The latest call and the speaking-coach baseline (`~/.config/palaver/coach-baseline.json`) are part of the summary.

## Waypoint (API only)

- Tools: `ticket.for_change` (by Claude session id, else repo toplevel path + branch, else a ticket key in the branch name), `ticket.link_case`, `ticket.get` (with links and sessions), `session.get` (repo path, cwd, branch, state, tmux window, ticket), plus the live stream.
- Waypoint's side is built later.

## Toolkit and API

- Every capability is a named tool with a description and params. The same registry is served over HTTP (`GET /api/tools`, `POST /api/tools/:name`) and the CLI (`cockpit tool NAME '{json}'`); an MCP server will expose it later.
- From a Claude session in the cockpit's tmux, the CLI passes the caller's pane, so pane tools act on the session's own window without a ticket key.
- Tools: settings.get/set, connections.status, project.list/overview/link_palaver/set_services/housekeep, ticket.get/add/update/set_project/links/link_meeting/auto_link_meetings/for_change/link_case, session.get, palaver.projects/meetings/coach, pane.list/run/logs/stop/up/layout, cockpit.summary.
- `GET /api/summary` (also `cockpit.summary`): attention (needs/turn), working, ready, done today, ticket counts, projects, focus (threads prompted in the last 30 min, prompts in 30 min, working streak in minutes without a 30-minute gap, forgotten sessions = your turn > 30 min, orphaned = doing tickets without a live session), calls (latest meeting, coach baseline).
- `GET /api/stream`: Server-Sent Events, a `summary` event on connect and whenever it changes.

## Cogitator (separate repo, branch from `proto/cockpit`)

- Reads the cockpit summary; when the cockpit doesn't answer, keeps the last summary for 15 s before falling back to its own hook log and tmux polling (avoids flipping and "restarting" the surfaces).
- Bar, right side: only what needs the user, one item per waiting session, longest first: `◉ KEY 43m` (needs) / `◐ KEY 12m` (your turn). No counts. Clicking selects the session in the cockpit (`@tk_select` on the cockpit tmux server) and brings the cockpit up (opens Ghostty with the cockpit if no client is attached).
- Wallpaper (orrery, arcane theme): rings per project (repo when no project), sessions as bodies (needs pulses red, your turn dims with waiting time, working orbits fast with a trail), ready tickets as dim moons by priority, closed-today tickets as a constellation, rings wobble with 3+ threads in 30 min, dust on forgotten sessions. Session constellations: one star per turn (prompt until Claude stops for the user), one constellation per session, named and flared when the session ends.
- The cat (Ollama) gets a work paragraph with every remark (what waits on the user, what's working, threads in 30 min, streak, closed today), may name tickets ("quests"), and nudges at most once per 5 minutes: 3+ threads in 30 min (cooldown 30 min), 90+ min streak → stretch (45 min), water every 60 min of activity, forgotten sessions (per ticket, 20 min), a session newly needing the user (90 s), a tip after each call of 5+ min using the coach baseline.

## Edge cases found during iteration

- Inherited `CLAUDE_CODE_CHILD_SESSION` disables transcript saving; inherited `PWD` started a session in the wrong repo.
- The last transcript entry can be an AskUserQuestion tool call.
- Sessions started before the hooks existed are invisible; killed non-tmux sessions keep their last state.
- Pane ids collide between tmux servers: cockpit panes are stored as `cockpit:%N`; the daemon must address the default server explicitly because it runs inside the cockpit's tmux.
- Pane ids in tmux formats: `%N` is not usable inside `#{==:…}` comparisons; mark panes with options (`@tk_queue`, `@tk_name`, `@tk_state`) instead.
- `send-keys` bypasses key bindings, so bindings can't be tested by injecting keys.
- All clients of a tmux session share one current window; detaching "the client showing the queue" detaches every client of the session.
- Pasting into a session that shows a question lands in the question's free-text option.
- Long-running Bash in a session was replaced by panes only after Claude was told so at session start.
- The cockpit summary occasionally takes ~0.6 s (git lookups); consumers need a timeout above that.
- AeroSpace owns nearly all Alt combinations; the cat persona must allow ticket keys.

## Rejected

- Linear and hosted trackers (remote); adopting Backlog.md / Beads.
- A Ghostty window per session; sessions run in tmux.
- The tracker creating worktrees (the session decides).
- Session-centric list as the only view (the original web kanban was the first frontend; the TUI replaced it as the main one).
- A desktop-app-first design (the user mostly works in tmux).
- Swapping single panes into the cockpit (replaced by the travelling queue, so a ticket's service panes stay visible).
- Zooming the session on Enter (the queue must stay visible).
- `Alt+q` (AeroSpace), `Ctrl+]` and Esc-to-queue as the queue key; Shift+Esc with a configurable fallback instead.
- Esc always going to the queue (kept as Claude's key; a confirmation on working sessions instead).
- A reply popup with quick-reply suggestions (hints under the Claude pane instead).
- Ready-ticket and working counts in the bar (noise).
- One constellation star per tool call; closing a constellation at every turn.
- Housekeeping or meeting search implementing anything.

## Robustness (2026-11-18)

- `ticket.start` is idempotent: Claude still running in the ticket's window → nothing starts (`started:false, reason:"already running"`, the TUI says "TIC-1 is already running" and moves there); Claude exited (the window is left with a shell) → Claude is respawned in the same pane with the same command, directory and environment (`restarted:true`, event `restarted`); no window → created. The same holds for the housekeeping and meeting windows. Running is told by `@tk_run` on the pane (set by the launch script), a live session in the DB, or, for windows of older builds, a foreground command that is not a shell.
- A ticket with a window counts as having a window even when its session ended: the projects tab's `⏎` and the right-side follow go to that window. `/api/state` and `/api/summary` carry `windows` (name, pane, claude).
- Build identity: mtime and size of the executable, read at startup (`COCKPIT_BUILD` overrides it for tests). `/api/health` and `cockpit daemon status` show it; the queue marks its pane `@tk_build`. `cockpit` (launcher) replaces a queue whose build differs or is missing; `cockpit install` restarts a daemon of another build; the TUI shows "daemon is a different build: cockpit daemon restart".
- The TUI never shows an HTTP status: a 404 reads "the daemon doesn't know <tool>: restart it (cockpit daemon restart)".
- `s` (queue or projects) calls `ticket.start`, switches to the queue tab and follows the ticket: the queue travels into its window at once and selects the first live session row of that ticket when the hook creates it (also after a restart or when Claude was already running; a key press or two minutes end the wait). `S` only starts it (flash "started KEY"), changing neither tab, selection nor focus.
