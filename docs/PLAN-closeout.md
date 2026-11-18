# Close-out, self-improvement, reload, update, deployment: implementation plan

Implements `REQUIREMENTS-closeout.md`. The prototype (`proto/closeout`, 332ce3e) is evidence; the build starts from `main`.

Branch `feat/closeout` in a fresh worktree `../ticket-cockpit-closeout`, off up-to-date `main`. The repo has a remote, so it ends as a PR.

## Code

| Module | Responsibility |
|---|---|
| `db.rs` | Migration v3: `errors(at, source, text, context)`, `close_requests(ticket, session, at)`. |
| `closeout.rs` (new) | `close_out(ctx, key, mode, summary)`: wrap (safe send, record request only on success; a refusal is a normal result) / now / keep / drop. `finish_requested(ctx)`: done + not working → stop service panes, end session, kill window, event. Moves the queue home before killing the window it sits in. |
| `errors.rs` (new) | Record and list errors; `tools::call` records every failed tool call in one place; `http.rs` records its own failures (bad JSON, failed sends). Expected refusals are excluded by type (`SendRefused`). |
| `tools.rs` | `ticket.close_out`, `cockpit.log_error`, `cockpit.errors`, `cockpit.feedback` (context section, empty text → last error), `cockpit.update_status`, `cockpit.reload`, `cockpit.update`. |
| `sessions.rs` | SessionStart context: the friction line. |
| `projects.rs` / housekeeping | Errors in the housekeeping prompt for the cockpit project. |
| `settings.rs` | `cockpit_project`, `cockpit_repo`, `check_updates`, `update_channel`, `release_repo`, with desc and help texts. |
| `update/mod.rs` (new) | Build identity (commit from `build.rs`, `COCKPIT_GIT_SHA` override), install-location guard, `status()` per channel, the window script (final lines and "press any key"), reload. |
| `update/local.rs` | `git rev-list --count`, hourly `git fetch`, behind-origin detection, `cargo install` + `cockpit install` steps. |
| `update/release.rs` | Manifest fetch via `/usr/bin/curl` (no TLS crate), version comparison, download, sha256 (`sha2` crate), unpack (`tar`), atomic replace + `cockpit.prev`, rollback, `--stage`. |
| `daemon.rs` | One scheduler thread: close-out watcher every 2 s, update check on start and every 10 min. `cockpit.update_status` reads the cached result. |
| `cli.rs` | `cockpit reload [--dry-run]`, `cockpit update [--dry-run] [--stage DIR]`, `cockpit rollback [--dry-run]`. |
| `tui/` | `C` close-out picker + summary input, `!` input, `R`/`U` confirmations, select the update/reload window after creating it, footer update line (lower priority than flashes), settings Update entry with right-side commits/notes, hint chips. |
| `build.rs` (new) | Embed `git rev-parse --short HEAD` (fallback "unknown"). |
| `.github/workflows/ci.yml`, `release.yml`, `install.sh` | As specified; `release.yml` writes `manifest.json`. |
| `README.md`, `docs/REQUIREMENTS.md` | Curl install first, update/reload/rollback, channels, version flow; append the new requirements. |

New dependency: `sha2` (small, standard). Downloads use `/usr/bin/curl`, which every Mac has, to avoid a TLS stack in the binary.

## Tests

- **close-out** (private tmux socket, `COCKPIT_CLAUDE` stand-in):
  - each mode's ticket/session/window outcome;
  - a wrap refused by a draft records nothing;
  - the watcher closes only after done + not working, and stops service panes;
  - the queue is moved home before its window is killed;
  - `cockpit close` uses the claiming session.
- **errors:**
  - a failing tool call (HTTP and CLI path) is recorded;
  - a send refusal is not;
  - `cockpit.errors` returns the last 50.
- **feedback:**
  - the ticket lands in `cockpit_project` with the Context section;
  - empty text files `Fix: <last error>`;
  - repo fallback.
- **update, local:**
  - temp git repo with commits ahead;
  - a remote clone behind origin → "pull first";
  - nothing to update.
- **update, release:**
  - manifest parsing and version/commit comparison;
  - sha256 mismatch stops;
  - stage → swap → rollback in a temp dir;
  - against a local HTTP server serving a fake manifest and asset (`COCKPIT_RELEASE_BASE`).
- **guards:**
  - a development build refuses update/rollback;
  - a `COCKPIT_DB` instance only prints reload steps.
- **window script:** final lines per outcome and the "press any key" tail (string-level), plus a tmux test that the window is selected and survives a 0 exit.
- **TUI** (`TestBackend`):
  - close-out picker order and default;
  - `!` input;
  - footer priority (flash beats update line);
  - settings Update entry.
- **install.sh:** run against the local fake release into a temp HOME (install, PATH warning, channel set, sha mismatch fails). `shellcheck` if installed.
- **Gate:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Steps (atomic commits)

1. Migration v3 + errors module + central error recording.
2. Close-out tool, watcher, queue-safe window kill, `cockpit close` session default.
3. Feedback tool, settings, SessionStart friction line, housekeeping errors.
4. Build identity + update status (local) + scheduler + `update_status`.
5. Release channel (manifest, download, verify, swap, rollback, stage).
6. Reload/update/rollback CLI + window script + guards.
7. TUI: `C`, `!`, `R`/`U`, footer, settings Update entry.
8. CI and release workflows, `install.sh`, README, requirements doc.

## After merge

1. Push the branch, open a PR, and you merge.
2. Install with `cargo install --path .` and `cockpit install`, then reload.
3. Delete the `proto/closeout` worktree and branch, and stop the isolated `cockpit-proto` setup (daemon on 4791 + tmux server).
4. First public release: bump to `0.2.0` and push tag `v0.2.0`, so CI builds the first release and `install.sh` works for others. This step publishes binaries and needs your go.

## Decisions

1. First release version: 0.2.0 (decided 2026-11-18).
