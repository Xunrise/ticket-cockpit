# Cockpit: close-out, self-improvement, reload, update and deployment — requirements

Derived from the prototype on branch `proto/closeout` (commit 332ce3e) and the conversation of 2026-11-18. Extends `docs/REQUIREMENTS.md`.

## Close out a session (`C`)

- `C` on a queue row with a ticket, or on a ticket in the projects tab, opens a picker "Close out KEY" with, in this order:
  1. **Wrap up and close** (default; Enter selects it)
  2. **Close now**
  3. **Close window, keep ticket**
  4. **Drop ticket**
- The hint chips show `[C] close out`.
- One daemon tool does the work: `ticket.close_out {key, mode: wrap|now|keep|drop, summary?}`.

### Wrap up and close

- Sends the session, through the safe-send rules, the text: `Wrap up KEY: finish what this ticket still needs (commit if appropriate), then close it with: cockpit close KEY --summary "<one paragraph>"`.
- If sending is refused (the user's draft is in the prompt, a question is open), nothing is recorded and the refusal is shown, e.g. `not sent: the prompt already has text in it ("…"): attach to send or clear it`. The refusal is a normal result, not an error.
- Otherwise the request is recorded and the flash reads `asked KEY's session to wrap up; the window closes when it is done`.
- The daemon (not the TUI) checks every 2 s. Once the ticket is `done` and its session is no longer working (your turn or ended), it:
  - stops the ticket's service panes,
  - marks the session ended,
  - kills the window,
  - logs the event "window closed after wrap-up".
- `cockpit close` defaults to the ticket's claiming session, so the branch and commits are recorded.

### Close now / keep / drop

- **Close now:** asks `summary for KEY (may be empty) ›`, marks the ticket done with that summary (branch and commits recorded), marks the session ended, kills the window.
- **Close window, keep ticket:** ticket back to `todo`, session ended, window killed.
- **Drop ticket:** ticket `canceled`, window killed.

### Applies to every mode

- If the queue sits in the window being closed, it moves to the home window first; the queue never dies with a ticket window.

## Improve the cockpit from inside

- Settings:
  - `cockpit_project` (default `ticket-cockpit`): where improvement tickets go;
  - `cockpit_repo` (default `~/Projects/ticket-cockpit`): the cockpit's source checkout.
- **Error log:** every failed tool call (HTTP and CLI, so Claude sessions' calls too) and every daemon HTTP error (bad JSON, failed pane send) is recorded with time, source, text and context.
  - Expected refusals (draft in the prompt, open question) are not errors.
  - Tools: `cockpit.log_error {text, context}`, `cockpit.errors` (the last 50).
- **`!`** anywhere in the TUI (not while typing) opens `improve the cockpit ›`:
  - With text: tool `cockpit.feedback {text, context}` creates a ticket in `cockpit_project` (the project's home repo, falling back to the repo named after `cockpit_repo`). The body is the text plus a "Context" section: tab, selected row or ticket, current flash, last error, build. Flash `filed TIC-N`.
  - Empty: files the last error as `Fix: <error>` with the same context.
- **Sessions report friction:** every session's SessionStart context includes one line: if you hit friction or a bug in the cockpit itself, file it with `cockpit tool cockpit.feedback '{"text":"…"}'` instead of working around it.
- **Housekeeping** on the cockpit project includes the recent errors, so repeated errors become tickets.

## Reload (`R`, `cockpit reload`)

- Restarts the daemon (`launchctl kickstart -k`) and replaces the queue with the currently **installed** binary. No build.
- `R` asks `Reload the cockpit (restart the daemon, replace the queue with the installed binary)? (y/n)`.
- On y it runs in a visible window `cockpit-reload` (see "Update and reload windows").
- `--dry-run` prints the steps.

## Update (`U`, `cockpit update`) and rollback

- Updates from the configured channel, then reloads.
- `U` asks `Update the cockpit (<channel>)? It builds/downloads in a window, then reloads. (y/n)`.
- On y it runs in a visible window `cockpit-update`.
- **Local channel:** in `cockpit_repo`: show `git log --oneline -1`, `cargo install --path .`, `cockpit install`, then reload.
- **Release channel:**
  - read the manifest;
  - download this machine's asset;
  - verify its sha256 (mismatch stops with `sha256 mismatch … expected …, got …`);
  - unpack;
  - replace the installed binary atomically, keeping the previous one as `cockpit.prev`;
  - reload.
- `cockpit rollback` swaps `cockpit.prev` back and reloads.
- `--dry-run` prints the steps (the release steps start with the manifest URL). `--stage DIR` downloads, verifies and unpacks into DIR without installing.
- **Safety:**
  - `update` and `rollback` only act when the running binary is the installed one (`~/.cargo/bin/cockpit`, `~/.local/bin/cockpit`, or the path the launchd agent records). Any other build refuses with `this is a development build (<path>); update applies to the installed cockpit at <path>`.
  - `reload` from an instance with `COCKPIT_DB` set only prints its steps, so a test setup can never restart the real daemon (opt out with `COCKPIT_ALLOW_REAL_UPDATE`).

## Update and reload windows

- The window is created and then selected, so the user watches the run. The queue may stay where it is.
- It never disappears silently. It ends with a clear last line, then `press any key to close`. Examples:
  - `update done: reloading…`
  - `nothing to update: you run <commit>, main is <commit>` (or the release equivalent)
  - the error plus `update failed (status N)`
- Exception: the successful path execs straight into the new queue.
- The window inherits the cockpit's environment: `COCKPIT_DB`, `COCKPIT_URL`, `COCKPIT_TMUX_SOCKET`, `COCKPIT_GIT_SHA`, `COCKPIT_RELEASE_BASE`. It reports the same state the queue showed.

## Update check and recommendation

- Every build embeds the git commit it was built from (`build.rs`; `COCKPIT_GIT_SHA` overrides it at runtime). `/api/health` and `cockpit daemon status` show it.
- Setting `check_updates` (default on). The daemon checks on start and every 10 minutes:
  - **Local channel:** commits on `cockpit_repo`'s `main` since the installed commit (`git rev-list --count <installed>..main`). If the repo has a remote: `git fetch` at most hourly, and "pull first" when local `main` is behind `origin/main`.
  - **Release channel:** the manifest's version. Newer means a higher version, or the same version with a different commit.
- Tool `cockpit.update_status` → `{installed, available, commits_ahead, behind_remote, summary, notes}`. The summary gains `update: {available, commits}` (cogitator may show it later; it is not changed now).
- **Recommendation when an update is available:**
  - The queue footer shows `update available: N commits · U to update`. Flash messages and errors take priority.
  - The settings tab gets an Update entry at the top:
    - It shows the channel and the installed version/commit (`local · 0.1.0 (dc34153)`) plus the availability line.
    - Chips: `[U] update [R] reload`; Enter asks to update.
    - The right side lists the waiting commits (local) or the release notes (release).

## Deployment (two channels)

- Setting `update_channel`: `local` | `release`.
  - `local` is the default for a development install (`cargo install --path .`).
  - `release` is set by `install.sh`.
- Setting `release_repo` (default `Xunrise/ticket-cockpit`). The repository is **public**; releases need no authentication (no `gh` dependency).
- **Manifest:** `https://github.com/<release_repo>/releases/latest/download/manifest.json` (`COCKPIT_RELEASE_BASE` overrides the base for tests):
  ```
  {version, commit, published, notes, assets: {"aarch64-apple-darwin": {url, sha256, size}, "x86_64-apple-darwin": {…}}}
  ```
- **CI** (`.github/workflows/ci.yml`): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` on push and PR.
- **Release** (`.github/workflows/release.yml`), on tags `v*`:
  - tests and clippy first;
  - builds `aarch64-apple-darwin` and `x86_64-apple-darwin` on macOS;
  - packages `cockpit-<target>.tar.gz` plus `.sha256`;
  - generates `manifest.json` (notes from `git log` since the previous tag);
  - creates the GitHub Release with all of them.
- **Install:** `install.sh` at the repo root, run as `curl -fsSL https://raw.githubusercontent.com/Xunrise/ticket-cockpit/main/install.sh | sh`. It:
  - detects the architecture and reads the manifest;
  - downloads the asset and verifies its sha256;
  - installs to `~/.local/bin/cockpit` (creates the directory, warns when it isn't on PATH, keeps `cockpit.prev`);
  - sets `update_channel=release`;
  - runs `cockpit install`.
- **Version flow** (README): bump `Cargo.toml`, tag `vX.Y.Z`, push the tag.
- The README leads with the curl install; `cargo install --path .` is the development route.

## Edge cases found during iteration

- A detached update window that exits 0 ("nothing to update") vanished before the user saw it.
- An update started from a development build could `cargo install` over the user's real binary. It was stopped in time; the guard prevents it now.
- The update window didn't get the cockpit's environment and so reported a different state than the queue.
- Killing the window the queue sat in would have killed the queue.
- A wrap-up request must not be recorded when sending is refused, or the window would close after an unrelated close.
- The `C` chip is clipped on a narrow queue.
- After an update pane closes, the queue stays alone in the update window until another row is picked (existing "stay" behaviour).

## Rejected

- One combined update-and-reload action (split into reload and update).
- Automatic updates (the user runs them; the cockpit checks and recommends).
- A private repository with `gh`-authenticated releases, or a private source repo plus a public release repo (the source repo is public, like Palaver).
- Logging expected send refusals as errors.
