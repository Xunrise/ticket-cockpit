//! Ticket windows in the cockpit's tmux session, named service panes and their layout.
use crate::tickets;
use crate::tmux::{Server, Tmux, HOME_WINDOW, SESSION};
use crate::{projects, settings};
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

const CK: Server = Server::Cockpit;
/// Inherited Claude variables disable transcript saving in a child session, so they never reach a new pane.
const UNSET_CLAUDE: &str = r#"for v in $(env | grep -o "^CLAUDE[A-Z_]*"); do unset $v; done"#;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PaneInfo {
    pub id: String,
    pub name: Option<String>,
    pub command: String,
    pub cwd: String,
    pub dead: bool,
    pub queue: bool,
    /// `@tk_run`: "1" while the window's launch script runs Claude, "0" once Claude has exited, empty for windows
    /// started by an older build.
    pub run: String,
}

/// A window of the cockpit session with its Claude pane (`cockpit:%N`) and whether Claude still runs in it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WindowInfo {
    pub name: String,
    pub pane: String,
    pub claude: bool,
}

/// Which window a pane tool acts on: an explicit ticket key wins over the caller's own pane.
#[derive(Debug, Default, Clone)]
pub struct Sel {
    pub key: Option<String>,
    pub caller_pane: Option<String>,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The launching process's environment minus `CLAUDE*`, with `PWD` set to the target directory
/// (an inherited PWD starts the session in the wrong repo).
pub fn clean_env(path: &str) -> Vec<(String, String)> {
    let mut env: Vec<_> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("CLAUDE") && k != "TMUX" && k != "TMUX_PANE" && k != "PWD")
        .collect();
    env.push(("PWD".into(), path.into()));
    env
}

/// Creates the window `name` in the cockpit session (and the session, with `cmd` as its first window).
/// Returns false when the window already exists.
pub fn cockpit_window(tmux: &Tmux, name: &str, path: &str, env: &[(&str, &str)], cmd: &str) -> Result<bool> {
    let session_exists = tmux.has_session(CK, SESSION);
    if session_exists && window_exists(tmux, name) {
        return Ok(false);
    }
    let mut a: Vec<String> = if session_exists {
        vec!["new-window".into(), "-d".into(), "-t".into(), format!("={SESSION}:")]
    } else {
        ["new-session", "-d", "-s", SESSION, "-x", "220", "-y", "60"].map(String::from).to_vec()
    };
    a.extend(["-n".into(), name.into(), "-c".into(), path.into()]);
    for (k, v) in env {
        a.extend(["-e".into(), format!("{k}={v}")]);
    }
    a.push(cmd.into());
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    tmux.run_env(CK, &a, clean_env(path))?;
    Ok(true)
}

pub fn window_exists(tmux: &Tmux, name: &str) -> bool {
    tmux.quiet(CK, &["list-windows", "-t", &format!("={SESSION}"), "-F", "#{window_name}"]).lines().any(|w| w == name)
}

/// Records in the pane whether Claude runs. The pane's foreground command cannot tell: the launch script's
/// own shell stays the process group leader while Claude runs, so tmux reports `zsh` either way.
const MARK: &str = "tmux set -p @tk_run";

/// The command a session window runs: `claude "$TK_PROMPT"` in a login shell without inherited Claude
/// variables; the window keeps a shell afterwards.
fn claude_cmd(claude: &str) -> String {
    format!(
        "/bin/zsh -lc {}",
        sh_quote(&format!(r#"{UNSET_CLAUDE}; {MARK} 1; {claude} "$TK_PROMPT"; {MARK} 0; exec zsh"#))
    )
}

/// COCKPIT_CLAUDE replaces the `claude` binary (tests use a stand-in).
fn claude_bin() -> String {
    std::env::var("COCKPIT_CLAUDE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "claude".into())
}

fn is_shell(cmd: &str) -> bool {
    matches!(cmd.trim_start_matches('-'), "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh" | "tcsh")
}

/// The window's Claude pane: the first one that is neither the queue nor a named service pane.
fn claude_pane(tmux: &Tmux, name: &str) -> Option<PaneInfo> {
    panes_in(tmux, &format!("={SESSION}:{name}")).ok()?.into_iter().find(|p| p.name.is_none())
}

/// Claude runs in the pane: the launch script says so; for windows of an older build, a foreground command that is
/// not a shell does. A live session of the DB counts as running too.
fn running(p: &PaneInfo, db_live: bool) -> bool {
    !p.dead
        && (db_live
            || match p.run.as_str() {
                "1" => true,
                "0" => false,
                _ => !is_shell(&p.command),
            })
}

/// The ticket's claiming session, or any session recorded in this pane, is live per the DB.
fn session_live(conn: &Connection, session: Option<&str>, pane: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sessions WHERE (id=?1 OR tmux_pane=?2) AND state IN ('needs','turn','working') AND ended IS NULL",
        rusqlite::params![session.unwrap_or(""), format!("cockpit:{pane}")],
        |_| Ok(()),
    )
    .is_ok()
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Launch {
    Created,
    Restarted,
    /// Claude is still running in the window: nothing was started.
    Running,
}

/// Makes sure Claude runs in the window `name`: creates the window, restarts Claude in it when it has exited
/// (same command, directory and environment as a fresh start), or leaves a running Claude alone.
fn ensure_claude(
    conn: &Connection,
    tmux: &Tmux,
    name: &str,
    path: &str,
    env: &[(&str, &str)],
    cmd: &str,
    session: Option<&str>,
) -> Result<Launch> {
    if cockpit_window(tmux, name, path, env, cmd)? {
        return Ok(Launch::Created);
    }
    let pane = claude_pane(tmux, name).ok_or_else(|| anyhow!("{name} has no Claude pane"))?;
    if running(&pane, session_live(conn, session, &pane.id)) {
        return Ok(Launch::Running);
    }
    let mut a: Vec<String> = ["respawn-pane", "-k", "-t", &pane.id, "-c", path].map(String::from).to_vec();
    for (k, v) in env {
        a.extend(["-e".into(), format!("{k}={v}")]);
    }
    a.push(cmd.into());
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    tmux.run_env(CK, &a, clean_env(path))?;
    Ok(Launch::Restarted)
}

/// Starts an interactive Claude session in a window of its own, without claiming a ticket.
/// An existing window is reused; Claude is restarted in it when it has exited.
pub fn run_claude(conn: &Connection, tmux: &Tmux, repo: &str, name: &str, prompt: &str) -> Result<String> {
    let path = tickets::repo_path(conn, repo)?.ok_or_else(|| anyhow!("unknown repo path for {repo}"))?;
    ensure_claude(conn, tmux, name, &path, &[("TK_PROMPT", prompt)], &claude_cmd(&claude_bin()), None)?;
    Ok(format!("cockpit:{name}"))
}

/// Starts the ticket's session: window named after the key in the repo's main path, `TK_ISSUE` set.
/// Idempotent: a running Claude is left alone (`started: false`), an exited one is restarted in its window.
pub fn start_ticket(conn: &Connection, tmux: &Tmux, key: &str) -> Result<Value> {
    start_ticket_with(conn, tmux, key, &claude_bin())
}

fn start_ticket_with(conn: &Connection, tmux: &Tmux, key: &str, claude: &str) -> Result<Value> {
    let t = tickets::get(conn, key)?;
    let path = tickets::repo_path(conn, &t.repo)?.ok_or_else(|| anyhow!("unknown repo path for {}", t.repo))?;
    let prompt = format!("Work on ticket {}: {}. Read it with the tracker's show command first.", t.key, t.title);
    let env = [("TK_ISSUE", t.key.as_str()), ("TK_PROMPT", prompt.as_str())];
    let window = format!("cockpit:{}", t.key);
    match ensure_claude(conn, tmux, &t.key, &path, &env, &claude_cmd(claude), t.session.as_deref())? {
        Launch::Running => Ok(json!({"window": window, "started": false, "reason": "already running"})),
        Launch::Restarted => {
            tickets::event(conn, key, None, "restarted", &format!("tmux {window}"))?;
            Ok(json!({"window": window, "started": true, "restarted": true}))
        }
        Launch::Created => {
            tickets::event(conn, key, None, "dispatched", &format!("tmux {window}"))?;
            if settings::get_bool(conn, "start_services_on_dispatch") && t.project.is_some() {
                let _ = up(conn, tmux, &Sel { key: Some(t.key.clone()), caller_pane: None });
                // no defaults for the project: nothing to start
            }
            Ok(json!({"window": window, "started": true}))
        }
    }
}

pub fn window_id(tmux: &Tmux, sel: &Sel) -> Result<String> {
    if let Some(key) = &sel.key {
        return tmux
            .run(CK, &["display", "-p", "-t", &format!("={SESSION}:{key}"), "#{window_id}"])
            .map_err(|_| anyhow!("no window for {key}"));
    }
    if let Some(p) = &sel.caller_pane {
        let id = p.strip_prefix("cockpit:").unwrap_or(p);
        return tmux.run(CK, &["display", "-p", "-t", id, "#{window_id}"]);
    }
    bail!("give a ticket key (or run inside a cockpit pane)")
}

/// Panes of a window; the queue pane visits windows and is not part of them.
pub fn panes_in(tmux: &Tmux, window: &str) -> Result<Vec<PaneInfo>> {
    let fmt = "#{pane_id}\t#{@tk_name}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}\t#{@tk_queue}\t#{@tk_run}";
    let out = tmux.run(CK, &["list-panes", "-t", window, "-F", fmt])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            PaneInfo {
                id: f[0].into(),
                name: Some(f[1]).filter(|n| !n.is_empty()).map(String::from),
                command: f[2].into(),
                cwd: f[3].into(),
                dead: f[4] == "1",
                queue: f[5] == "1",
                run: f[6].into(),
            }
        })
        .filter(|p| !p.queue)
        .collect())
}

/// Every window of the cockpit session except the home window, each with its Claude pane.
pub fn windows(tmux: &Tmux) -> Vec<WindowInfo> {
    let fmt =
        "#{window_name}\t#{pane_id}\t#{@tk_queue}\t#{@tk_name}\t#{pane_current_command}\t#{pane_dead}\t#{@tk_run}";
    let out = tmux.quiet(CK, &["list-panes", "-s", "-t", &format!("={SESSION}"), "-F", fmt]);
    let mut v: Vec<WindowInfo> = vec![];
    for l in out.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 7 || f[0] == HOME_WINDOW || f[2] == "1" || !f[3].is_empty() || v.iter().any(|w| w.name == f[0]) {
            continue;
        }
        let p = PaneInfo {
            id: f[1].into(),
            name: None,
            command: f[4].into(),
            cwd: String::new(),
            dead: f[5] == "1",
            queue: false,
            run: f[6].into(),
        };
        v.push(WindowInfo { name: f[0].into(), pane: format!("cockpit:{}", p.id), claude: running(&p, false) });
    }
    v
}

fn pane_named(tmux: &Tmux, window: &str, name: &str) -> Result<Option<PaneInfo>> {
    Ok(panes_in(tmux, window)?.into_iter().find(|p| p.name.as_deref() == Some(name)))
}

fn service_shell(name: &str, cmd: &str) -> String {
    let script = format!(r#"{UNSET_CLAUDE}; ({cmd}); code=$?; echo; echo "[{name} exited: $code]"; exec zsh"#);
    format!("/bin/zsh -lc {}", sh_quote(&script))
}

/// Runs a command in the named pane of a window; an existing pane of that name is respawned.
pub fn run(tmux: &Tmux, sel: &Sel, name: &str, cmd: &str, cwd: Option<&str>) -> Result<Value> {
    if name.is_empty() || cmd.is_empty() {
        bail!("name and cmd are required");
    }
    let w = window_id(tmux, sel)?;
    let panes = panes_in(tmux, &w)?;
    let cwd = match cwd {
        Some(c) => c.to_string(),
        None => panes.first().map(|p| p.cwd.clone()).ok_or_else(|| anyhow!("window has no panes"))?,
    };
    let shell = service_shell(name, cmd);
    match pane_named(tmux, &w, name)? {
        Some(old) => {
            tmux.run(CK, &["respawn-pane", "-k", "-t", &old.id, "-c", &cwd, &shell])?;
        }
        None => {
            let last = panes.last().ok_or_else(|| anyhow!("window has no panes"))?;
            let id = tmux
                .run(CK, &["split-window", "-d", "-v", "-t", &last.id, "-c", &cwd, "-P", "-F", "#{pane_id}", &shell])?;
            tmux.run(CK, &["set", "-p", "-t", &id, "@tk_name", name])?;
        }
    }
    layout(tmux, &w)?;
    Ok(json!({"window": w, "name": name, "cmd": cmd, "cwd": cwd}))
}

/// Claude on top at full width, services side by side in a strip below it (30% of the height).
pub fn layout(tmux: &Tmux, window: &str) -> Result<()> {
    let panes = panes_in(tmux, window)?;
    let Some((main, services)) = panes.split_first() else {
        return Ok(());
    };
    if services.is_empty() {
        return Ok(());
    }
    tmux.run(CK, &["join-pane", "-d", "-v", "-l", "30%", "-s", &services[0].id, "-t", &main.id])?;
    for (i, p) in services.iter().enumerate().skip(1) {
        tmux.run(CK, &["join-pane", "-d", "-h", "-s", &p.id, "-t", &services[i - 1].id])?;
    }
    // ponytail: each -h split halves the previous service (50/25/25 for three); even them out if more than two get common
    Ok(())
}

pub fn list(tmux: &Tmux, sel: &Sel) -> Result<Vec<PaneInfo>> {
    panes_in(tmux, &window_id(tmux, sel)?)
}

pub fn logs(tmux: &Tmux, sel: &Sel, name: &str, lines: u32) -> Result<String> {
    let w = window_id(tmux, sel)?;
    let p = pane_named(tmux, &w, name)?.ok_or_else(|| anyhow!("no pane {name}"))?;
    tmux.run(CK, &["capture-pane", "-p", "-t", &p.id, "-S", &format!("-{lines}")])
}

pub fn stop(tmux: &Tmux, sel: &Sel, name: &str) -> Result<Value> {
    let w = window_id(tmux, sel)?;
    let p = pane_named(tmux, &w, name)?.ok_or_else(|| anyhow!("no pane {name}"))?;
    tmux.run(CK, &["kill-pane", "-t", &p.id])?;
    Ok(json!({"stopped": name}))
}

/// Starts the ticket project's default panes. The key defaults to `TK_ISSUE` of the calling process.
pub fn up(conn: &Connection, tmux: &Tmux, sel: &Sel) -> Result<Value> {
    let key = sel.key.clone().or_else(|| std::env::var("TK_ISSUE").ok()).ok_or_else(|| anyhow!("ticket key needed"))?;
    let project = tickets::get(conn, &key)?.project.ok_or_else(|| anyhow!("{key} has no project"))?;
    let services = projects::services(conn, &project)?;
    if services.is_empty() {
        bail!("project {project} has no services; set them with project.set_services");
    }
    let sel = Sel { key: Some(key), caller_pane: sel.caller_pane.clone() };
    let base = panes_in(tmux, &window_id(tmux, &sel)?)?.first().map(|p| p.cwd.clone()).unwrap_or_default();
    let started = services
        .iter()
        .map(|sv| {
            let cwd = sv
                .cwd
                .as_ref()
                .map(|c| Path::new(&base).join(c).to_string_lossy().into_owned())
                .unwrap_or_else(|| base.clone());
            run(tmux, &sel, &sv.name, &sv.cmd, Some(&cwd))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(started))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::temp_db;
    use crate::tmux::testutil::{guard, wait_until};

    fn field(t: &Tmux, pane: &str, f: &str) -> i64 {
        t.run(CK, &["display", "-p", "-t", pane, &format!("#{{{f}}}")]).unwrap().parse().unwrap()
    }

    #[test]
    fn starting_a_ticket_opens_its_window_once() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        std::env::set_var("COCKPIT_CLAUDE", "true"); // never launch the real claude from tests
        let (_d, c) = temp_db();
        let repo = std::env::temp_dir();
        c.execute("INSERT INTO repos(name,path) VALUES('demo', ?)", [repo.to_string_lossy()]).unwrap();
        let key = tickets::create(
            &c,
            tickets::NewTicket { repo: "demo".into(), title: "Start me".into(), ..Default::default() },
        )
        .unwrap();
        let r = start_ticket(&c, t, &key).unwrap();
        assert_eq!(
            (r["window"].as_str(), r["started"].as_bool()),
            (Some(format!("cockpit:{key}").as_str()), Some(true))
        );
        assert!(window_exists(t, &key));
        assert!(tickets::events(&c, &key).unwrap().iter().any(|e| e["kind"] == "dispatched"));
    }

    const SLEEPER: &str = "sh -c 'exec sleep 60' --";

    fn demo_ticket(c: &Connection) -> String {
        c.execute("INSERT INTO repos(name,path) VALUES('demo', ?)", [std::env::temp_dir().to_string_lossy()]).unwrap();
        tickets::create(c, tickets::NewTicket { repo: "demo".into(), title: "T".into(), ..Default::default() }).unwrap()
    }

    fn run_of(t: &Tmux, key: &str) -> String {
        claude_pane(t, key).map(|p| p.run).unwrap_or_default()
    }

    #[test]
    fn starting_a_running_ticket_does_nothing() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let (_d, c) = temp_db();
        let key = demo_ticket(&c);
        assert_eq!(start_ticket_with(&c, t, &key, SLEEPER).unwrap()["started"], true);
        assert!(wait_until(|| run_of(t, &key) == "1"));
        let pane = claude_pane(t, &key).unwrap().id;
        let r = start_ticket_with(&c, t, &key, SLEEPER).unwrap();
        assert_eq!(r, json!({"window": format!("cockpit:{key}"), "started": false, "reason": "already running"}));
        assert_eq!(claude_pane(t, &key).unwrap().id, pane);
        assert_eq!(run_of(t, &key), "1", "the running session was not touched");
        assert!(windows(t).iter().any(|w| w.name == key && w.claude));
    }

    #[test]
    fn starting_after_claude_exited_restarts_it_in_the_same_window() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let (d, c) = temp_db();
        let key = demo_ticket(&c);
        // `true` exits at once: the window is left with its shell
        assert_eq!(start_ticket_with(&c, t, &key, "true").unwrap()["started"], true);
        assert!(wait_until(|| run_of(t, &key) == "0"));
        let w = windows(t);
        assert_eq!((w.len(), w[0].name.as_str(), w[0].claude), (1, key.as_str(), false));
        let win = window_id(t, &Sel { key: Some(key.clone()), caller_pane: None }).unwrap();

        // the stand-in records what it was started with
        let out = d.path().join("env.txt");
        let probe = format!("sh -c 'echo \"$TK_ISSUE $TK_PROMPT $(pwd -P)\" > {}; exec sleep 60' --", out.display());
        let r = start_ticket_with(&c, t, &key, &probe).unwrap();
        assert_eq!(r, json!({"window": format!("cockpit:{key}"), "started": true, "restarted": true}));
        assert!(wait_until(|| run_of(t, &key) == "1"));
        let sel = Sel { key: Some(key.clone()), caller_pane: None };
        assert_eq!(window_id(t, &sel).unwrap(), win);
        assert_eq!(list(t, &sel).unwrap().len(), 1);
        assert!(tickets::events(&c, &key).unwrap().iter().any(|e| e["kind"] == "restarted"));
        assert!(wait_until(|| std::fs::metadata(&out).is_ok_and(|m| m.len() > 0)));
        let seen = std::fs::read_to_string(&out).unwrap();
        let repo = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(
            seen.trim(),
            format!("{key} Work on ticket {key}: T. Read it with the tracker's show command first. {}", repo.display())
        );
        assert!(windows(t)[0].claude);
    }

    #[test]
    fn run_claude_reuses_its_window_and_restarts_an_exited_claude() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let (_d, c) = temp_db();
        demo_ticket(&c);
        let path = tickets::repo_path(&c, "demo").unwrap().unwrap();
        ensure_claude(&c, t, "Demo-housekeeping", &path, &[("TK_PROMPT", "p")], &claude_cmd("true"), None).unwrap();
        assert!(wait_until(|| run_of(t, "Demo-housekeeping") == "0"));
        let again = ensure_claude(&c, t, "Demo-housekeeping", &path, &[("TK_PROMPT", "p")], &claude_cmd(SLEEPER), None)
            .unwrap();
        assert_eq!(again, Launch::Restarted);
        assert!(wait_until(|| run_of(t, "Demo-housekeeping") == "1"));
        let again = ensure_claude(&c, t, "Demo-housekeeping", &path, &[("TK_PROMPT", "p")], &claude_cmd(SLEEPER), None)
            .unwrap();
        assert_eq!(again, Launch::Running);
    }

    #[test]
    fn a_live_session_in_the_db_keeps_a_shell_window_running() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let (_d, c) = temp_db();
        let key = demo_ticket(&c);
        start_ticket_with(&c, t, &key, "true").unwrap();
        assert!(wait_until(|| run_of(t, &key) == "0"));
        c.execute("INSERT INTO sessions(id,repo,cwd,state) VALUES('s1','demo','/x','turn')", []).unwrap();
        c.execute("UPDATE tickets SET session='s1' WHERE key=?", [&key]).unwrap();
        assert_eq!(start_ticket_with(&c, t, &key, SLEEPER).unwrap()["started"], false);
        c.execute("UPDATE sessions SET state='ended' WHERE id='s1'", []).unwrap();
        assert_eq!(start_ticket_with(&c, t, &key, SLEEPER).unwrap()["restarted"], true);
    }

    #[test]
    fn window_per_ticket_and_named_service_panes() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        assert!(cockpit_window(t, "DEM-1", "/tmp", &[], "cat").unwrap());
        assert!(cockpit_window(t, "DEM-2", "/tmp", &[], "cat").unwrap());
        assert!(!cockpit_window(t, "DEM-2", "/tmp", &[], "cat").unwrap(), "same name reuses the window");
        assert!(window_exists(t, "DEM-1") && window_exists(t, "DEM-2"));
        assert!(t.run(CK, &["list-sessions", "-F", "#{session_name}"]).unwrap() == SESSION);

        let sel = Sel { key: Some("DEM-1".into()), caller_pane: None };
        run(t, &sel, "backend", "echo up-and-running; sleep 30", Some("/tmp")).unwrap();
        run(t, &sel, "web", "echo web; exit 3", None).unwrap();
        let panes = list(t, &sel).unwrap();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].name, None);
        let backend = panes.iter().find(|p| p.name.as_deref() == Some("backend")).unwrap().clone();
        let web = panes.iter().find(|p| p.name.as_deref() == Some("web")).unwrap().clone();

        // services form a strip below Claude
        let claude = &panes[0].id;
        assert!(field(t, &backend.id, "pane_top") > field(t, claude, "pane_top"));
        assert_eq!(field(t, &backend.id, "pane_top"), field(t, &web.id, "pane_top"));
        assert_ne!(field(t, &backend.id, "pane_left"), field(t, &web.id, "pane_left"));
        assert_eq!(field(t, claude, "pane_width"), 220);

        assert!(wait_until(|| logs(t, &sel, "backend", 20).unwrap().contains("up-and-running")));
        // the exit code is shown when the command ends, and the pane keeps a shell
        assert!(wait_until(|| logs(t, &sel, "web", 20).unwrap().contains("[web exited: 3]")));

        // running the same name respawns the pane instead of adding one
        run(t, &sel, "backend", "echo second-run; sleep 30", None).unwrap();
        assert_eq!(list(t, &sel).unwrap().len(), 3);
        assert!(wait_until(|| logs(t, &sel, "backend", 20).unwrap().contains("second-run")));

        stop(t, &sel, "web").unwrap();
        assert_eq!(list(t, &sel).unwrap().len(), 2);
        assert!(stop(t, &sel, "web").is_err());

        // a caller pane resolves to its own window
        let by_pane = Sel { key: None, caller_pane: Some(claude.clone()) };
        assert_eq!(window_id(t, &by_pane).unwrap(), window_id(t, &sel).unwrap());
        assert!(window_id(t, &Sel::default()).is_err());
    }

    #[test]
    fn queue_pane_is_not_a_window_pane() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        cockpit_window(t, "W", "/tmp", &[], "cat").unwrap();
        let sel = Sel { key: Some("W".into()), caller_pane: None };
        let w = window_id(t, &sel).unwrap();
        let id = t.run(CK, &["split-window", "-d", "-h", "-t", &w, "-P", "-F", "#{pane_id}", "cat"]).unwrap();
        t.set_pane_option(&crate::tmux::PaneRef::cockpit(&id), "@tk_queue", "1").unwrap();
        assert_eq!(list(t, &sel).unwrap().len(), 1);
    }

    #[test]
    fn pane_up_starts_project_services_relative_to_the_claude_pane() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let (d, c) = temp_db();
        let repo = d.path().join("proj");
        std::fs::create_dir_all(repo.join("web")).unwrap();
        c.execute("INSERT INTO repos VALUES('proj', ?)", [repo.to_str().unwrap()]).unwrap();
        let key = tickets::create(
            &c,
            tickets::NewTicket {
                repo: "proj".into(),
                title: "t".into(),
                project: Some("P".into()),
                ..Default::default()
            },
        )
        .unwrap();
        projects::set_services(
            &c,
            "P",
            &[projects::Service { name: "fe".into(), cmd: "pwd; sleep 30".into(), cwd: Some("web".into()) }],
        )
        .unwrap();
        cockpit_window(t, &key, repo.to_str().unwrap(), &[], "cat").unwrap();
        let sel = Sel { key: Some(key), caller_pane: None };
        up(&c, t, &sel).unwrap();
        assert!(wait_until(|| logs(t, &sel, "fe", 20).unwrap().contains("proj/web")));
        let no =
            tickets::create(&c, tickets::NewTicket { repo: "proj".into(), title: "x".into(), ..Default::default() })
                .unwrap();
        assert!(up(&c, t, &Sel { key: Some(no), caller_pane: None }).is_err());
    }

    #[test]
    fn env_is_cleaned() {
        let env = clean_env("/x");
        assert!(env.iter().all(|(k, _)| !k.starts_with("CLAUDE")));
        assert_eq!(env.iter().filter(|(k, _)| k == "PWD").count(), 1);
        assert!(env.contains(&("PWD".into(), "/x".into())));
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
    }
}
