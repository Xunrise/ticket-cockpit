# cockpit

Local hub for tickets, projects, Claude sessions and terminals. One binary, one SQLite file, a background daemon and a terminal UI that runs in its own tmux server. macOS and tmux required.

- Every Claude session, wherever it runs, reports its state through global hooks: working, needs you, your turn, ended.
- The queue lists what needs you first, then your turn, working sessions, ENDED tickets (in progress, window still open, Claude gone) and ready tickets. You answer sessions from the queue.
- Tickets belong to projects; projects carry a Palaver link and default dev-server panes.
- Claude sessions use the same tools through `cockpit tool`.

Design documents: `docs/REQUIREMENTS.md`, `docs/PLAN.md`.

## Install

```sh
cargo install --path .     # puts `cockpit` in ~/.cargo/bin
cockpit install            # tmux config, launchd agent, global Claude hooks
cockpit onboard            # seed projects from your recent Claude sessions
```

`cockpit install` is idempotent and prints what it changed. `--dry-run` prints the same report and writes nothing.

| Step | What it does |
|---|---|
| tmux config | `~/.config/cockpit/cockpit.tmux.conf` (for the cockpit's own tmux server; your `~/.tmux.conf` is not read) |
| launchd agent | `~/Library/LaunchAgents/io.cockpit.daemon.plist` runs `cockpit daemon run` (RunAtLoad, KeepAlive, absolute path of the current binary), logs to `~/Library/Application Support/cockpit/logs/daemon.log`; bootstrapped on install |
| Claude hooks | `~/.claude/settings.json`: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop and SessionEnd call `<path>/cockpit hook` (timeout 5). Other hooks and settings stay as they are (key order and indent kept); a timestamped backup (`settings.json.cockpit-backup-…`) is written next to the file first; entries of the prototype (`ticket-cockpit-proto/tk.mjs hook`) are replaced; re-running never duplicates. |

After moving or reinstalling the binary, run `cockpit install` again: it updates the plist and the hook commands.

### Onboarding

`cockpit onboard` is re-runnable and asks before it writes (`--yes` accepts the defaults without a terminal; `--days N` changes the 60-day window).

1. Scans `~/.claude/projects/*/*.jsonl` modified in the last 60 days. Only the `cwd` field of each entry is read, never message content. Worktrees resolve to their main repo; directories that are gone or not in git are skipped. Repos are listed with their session counts.
2. Proposes one project per repo. Commands: `rename N name`, `merge 1,3,4 [name]`, `skip N`, `list`, Enter to accept. Existing projects are left alone.
3. Offers to link each project to a Palaver project (default: the same name); skipped when Palaver is not available.
4. Offers to start a Claude session in the cockpit's tmux (window `onboarding`, or `--start-session`). It reads the recent transcripts of each project and proposes tickets for unfinished work and default service panes where dev servers were started. It files them only after you confirm in that session.

## Daily use

```sh
cockpit         # outside tmux: attach to (or create) the cockpit session; inside: the queue
```

`q` leaves (everything keeps running), `Q` stops the queue. Tabs: Queue, Projects, Settings (`Tab` / `Shift+Tab`; `,` opens settings; `?` shows the keys of the current tab).

**Queue**

| Key | Action |
|---|---|
| `j` / `k` | move |
| `Enter` | move the cursor into the session's pane (the queue stays next to it) |
| `1`-`4`, `a`, `x` | pick an option of a question, accept, interrupt |
| `r` | type a reply to the session |
| quick-reply keys | send a canned reply (setting `quick_replies`, default `c=continue; y=yes; t=try again; m=merge it; d=start the dev server`) |
| `S` | start a session in the background: no tab, selection or focus change (also in projects) |
| `s` | start a session for a ready ticket, switch to the queue and follow it until its row appears; on an ENDED row restart Claude in its window; on a running one, go there |
| `p`, `l` | set the ticket's project; link a Palaver meeting as context |

Replies are only sent when the prompt is empty (or shows Claude's grey suggestion) and no question is open; the cockpit checks the pane before and after and tells you when a draft of yours is in the way.

**Queue key.** From any pane of the cockpit, `Shift+Esc` (setting `queue_key`, tmux key names: `S-Escape`, `C-]`, `F12`) puts the cursor back on the queue.

**Esc confirmation.** `Esc` in a Claude pane that is working would cancel its task, so a small menu asks first: `i` interrupt, `k` keep running, `n` interrupt and never ask again. When Claude waits for you, `Esc` goes straight through. Setting `confirm_interrupt`.

**Pane hints.** The selected row and the session's pane border show the keys that apply to it (bold blue), so you do not have to remember them.

**Projects.** Pick a project to see planned, in-progress and done tickets by cluster. `/` filter, `n` new task, `g` set cluster, `s` start a ticket, `Enter` open its session, `o` switch project, `H` let a Claude session tidy the planned tickets (duplicates, clusters, titles, stale items), `Esc` back to the list. Default dev-server panes of a project (`project.set_services`) start under a ticket's Claude pane when `start_services_on_dispatch` is on.

**Settings.** Each setting has a description and a help text; `Enter` changes it, `r` resets it to the default. The same tab shows the connections (Claude hooks, Cogitator, Ollama, Palaver, Waypoint) with their status.

## Daemon

```sh
cockpit daemon status     # running, pid, port, db, uptime, build (exit 1 when not running)
cockpit daemon start | stop | restart
cockpit daemon logs [--n 100] [--follow]
cockpit daemon run        # in the foreground, what launchd starts (--port N, default 4747)
```

The daemon serves the HTTP API on `127.0.0.1:4747` (`/api/summary`, `/api/stream` (SSE), `/api/state`, `/api/tools`, `/api/tools/NAME`). Requests need a localhost `Host`; POSTs need `x-tk: 1`. The hook does not depend on it: it writes the database directly.

## How Claude sessions use it

SessionStart tells the session about the cockpit. Sessions call the tool registry from the shell:

```sh
cockpit tool project.overview '{"project":"Demo"}'
cockpit tool ticket.add '{"project":"Demo","title":"Handle empty input","body":"..."}'
cockpit tool pane.run '{"name":"web","cmd":"npm run dev"}'
cockpit tool project.set_services '{"project":"Demo","services":[{"name":"web","cmd":"npm run dev","cwd":"web"}]}'
```

`GET /api/tools` lists every tool with its parameters. Shortcuts: `cockpit add "title" ["body"]`, `ready [--all]`, `show KEY`, `claim KEY --session ID`, `comment KEY "text"`, `close KEY --summary "..."`. A session started from a ticket (`TK_ISSUE`) claims it automatically.

## Uninstall

```sh
cockpit uninstall          # --dry-run to preview
```

Removes the cockpit hooks from `~/.claude/settings.json` (backup first, other hooks untouched), boots the launchd agent out and deletes its plist. The config (`~/.config/cockpit`) and the database (`~/Library/Application Support/cockpit`) are kept; delete them by hand for a clean slate.

## Development

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

| Variable | Effect |
|---|---|
| `HOME` | every path derives from it (tests of install and onboard run on a temp one with a fake `launchctl` on `PATH`) |
| `COCKPIT_DB` | database file; context files live next to it (`<db>.context`) |
| `COCKPIT_URL` | daemon the TUI talks to (default `http://127.0.0.1:4747`) |
| `COCKPIT_TMUX_SOCKET` | the cockpit's tmux socket (default `cockpit`); use a private one for experiments |
| `PALAVER_BIN` | path of the Palaver CLI |

Validating against old data: `python3 -I scripts/proto-to-cockpit.py PROTO_DATA_DB OUT_DB` converts a copy of the prototype's database into the current schema (for tests, never for the real install). Run a daemon on another port against it: `COCKPIT_DB=OUT_DB COCKPIT_TMUX_SOCKET=scratch cockpit daemon run --port 4798`.

The integration tests that use tmux run on private sockets and skip when tmux is missing.
