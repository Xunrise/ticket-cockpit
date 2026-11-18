//! The terminal UI: the queue, projects and settings tabs. It talks to the daemon over HTTP and drives the
//! cockpit's own tmux server.
pub mod api;
pub mod app;
pub mod bindings;
pub mod projects;
pub mod queue;
pub mod settings;
pub mod travel;
pub mod widgets;

use crate::paths;
use crate::sessions::HookEnv;
use crate::tmux::{Server, Tmux, HOME_WINDOW, SESSION};
use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event};
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

/// The tmux config for the cockpit's server, embedded in the binary.
pub const TMUX_CONF: &str = include_str!("../../assets/cockpit.tmux.conf");

/// Writes the embedded tmux config when missing or changed. Returns its path.
pub fn write_tmux_conf() -> Result<std::path::PathBuf> {
    let path = paths::tmux_conf();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(TMUX_CONF) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, TMUX_CONF)?;
    }
    Ok(path)
}

const CK: Server = Server::Cockpit;

/// The queue panes among `(pane id, @tk_build)` that are not of build `mine`: a foreign or older queue
/// (one that predates build marks has an empty build).
pub fn stale_queues<'a>(queues: &'a [(String, String)], mine: &str) -> Vec<&'a str> {
    queues.iter().filter(|(_, b)| b != mine).map(|(id, _)| id.as_str()).collect()
}

/// Stops queues of another build in the cockpit session, leaving every other window alone: the queue pane is
/// killed (a killed process cannot do the `Q` cleanup itself) and so is the slot pane it left in the home
/// window, so the new queue opens a fresh one. Returns whether a queue was replaced.
pub fn replace_stale_queue(tmux: &Tmux, mine: &str) -> Result<bool> {
    let target = format!("={SESSION}");
    let fmt = "#{pane_id}\t#{@tk_queue}\t#{@tk_build}\t#{window_name}\t#{pane_current_command}\t#{@tk_name}";
    let out = tmux.quiet(CK, &["list-panes", "-s", "-t", &target, "-F", fmt]);
    let panes: Vec<Vec<&str>> =
        out.lines().map(|l| l.split('\t').collect()).filter(|f: &Vec<&str>| f.len() == 6).collect();
    let queues: Vec<(String, String)> =
        panes.iter().filter(|f| f[1] == "1").map(|f| (f[0].to_string(), f[2].to_string())).collect();
    let stale = stale_queues(&queues, mine);
    if stale.is_empty() {
        return Ok(false);
    }
    for id in &stale {
        tmux.run(CK, &["kill-pane", "-t", id])?;
    }
    // the stale queue's slot: the idle `tail` pane of the home window; spared when a current queue still needs it
    if stale.len() == queues.len() {
        for f in panes.iter().filter(|f| f[3] == HOME_WINDOW && f[1] != "1" && f[5].is_empty() && f[4] == "tail") {
            let _ = tmux.run(CK, &["kill-pane", "-t", f[0]]);
        }
    }
    Ok(true)
}

/// `cockpit` with no arguments: outside the cockpit's tmux, attach to (or create) the session; inside, run the queue.
pub fn launch() -> Result<()> {
    let conf = write_tmux_conf()?;
    let tmux = Tmux::system();
    let build = crate::util::build_id();
    if HookEnv::from_process().on_cockpit(&tmux) {
        if replace_stale_queue(&tmux, &build)? {
            println!("replaced an older queue");
        }
        return run_queue(tmux);
    }
    let exe = std::env::current_exe()?;
    // the session can outlive the queue (ticket windows keep running): bring the queue back first
    let t = Tmux { conf: Some(conf.clone()), ..tmux.clone() };
    if replace_stale_queue(&t, &build)? {
        println!("replaced an older queue");
    }
    if t.has_session(Server::Cockpit, SESSION) {
        let panes = t.quiet(Server::Cockpit, &["list-panes", "-s", "-t", &format!("={SESSION}"), "-F", "#{@tk_queue}"]);
        if !panes.lines().any(|l| l == "1") {
            let _ = t.run(Server::Cockpit, &["new-window", "-t", &format!("={SESSION}:"), &exe.display().to_string()]);
        }
    }
    let err = std::process::Command::new("tmux")
        .args(["-L", &t.cockpit_sock, "-f"])
        .arg(&conf)
        .args(["new-session", "-A", "-s", SESSION])
        .arg(&exe)
        .env_remove("TMUX")
        .exec();
    bail!("cannot start tmux ({err}); the cockpit needs tmux")
}

fn run_queue(tmux: Tmux) -> Result<()> {
    let Ok(me) = std::env::var("TMUX_PANE") else { bail!("TMUX_PANE is not set") };
    let travel = travel::Travel::start(tmux.clone(), &me)?;
    bindings::remove_legacy(&tmux);
    let mut app = app::App::new(api::Api::from_env(), tmux, Some(travel), &me);
    let mut term = ratatui::init();
    let mut last = Instant::now();
    let result = (|| -> Result<()> {
        while !app.quit {
            if app.refresh || last.elapsed() >= Duration::from_secs(1) {
                app.refresh = false;
                last = Instant::now();
                app.tick();
            }
            term.draw(|f| app.draw(f))?;
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(k) = event::read()? {
                    app.handle_key(k);
                }
            }
        }
        Ok(())
    })();
    app.shutdown();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::testutil::{guard, wait_until};

    #[test]
    fn queues_of_another_or_no_build_are_stale() {
        let q = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        assert!(stale_queues(&q(&[("%1", "b2")]), "b2").is_empty());
        assert_eq!(stale_queues(&q(&[("%1", "b1"), ("%2", "b2")]), "b2"), ["%1"]);
        assert_eq!(stale_queues(&q(&[("%1", "")]), "b2"), ["%1"], "an older queue has no mark");
        assert!(stale_queues(&[], "b2").is_empty());
    }

    fn pane_ids(t: &Tmux) -> String {
        t.quiet(CK, &["list-panes", "-s", "-t", &format!("={SESSION}"), "-F", "#{window_name}:#{pane_current_command}"])
    }

    /// Home window with a queue pane (build `build`, empty for none) and a slot, plus a ticket window.
    fn cockpit_with_queue(t: &Tmux, build: &str) -> String {
        t.run(CK, &["new-session", "-d", "-s", SESSION, "-x", "120", "-y", "40", "-n", HOME_WINDOW, "cat"]).unwrap();
        let q = t.run(CK, &["list-panes", "-t", SESSION, "-F", "#{pane_id}"]).unwrap();
        t.run(CK, &["set", "-p", "-t", &q, "@tk_queue", "1"]).unwrap();
        if !build.is_empty() {
            t.run(CK, &["set", "-p", "-t", &q, "@tk_build", build]).unwrap();
        }
        t.run(CK, &["split-window", "-d", "-h", "-t", &q, "exec tail -f /dev/null"]).unwrap();
        t.run(CK, &["new-window", "-d", "-t", &format!("={SESSION}:"), "-n", "DEM-1", "sleep 60"]).unwrap();
        assert!(wait_until(|| pane_ids(t).contains("tail")));
        q
    }

    #[test]
    fn a_queue_of_another_build_is_replaced_and_the_other_windows_survive() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        for old in ["old-build", ""] {
            cockpit_with_queue(t, old);
            assert!(replace_stale_queue(t, "new-build").unwrap(), "build {old:?}");
            let left = pane_ids(t);
            assert!(!left.contains("cat") && !left.contains("tail"), "queue and slot are gone: {left}");
            assert!(left.contains("DEM-1:"), "the ticket window survives: {left}");
            t.kill_server(CK);
            assert!(wait_until(|| !t.has_session(CK, SESSION)));
        }
    }

    #[test]
    fn a_queue_of_the_current_build_is_left_alone() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let q = cockpit_with_queue(t, "same");
        assert!(!replace_stale_queue(t, "same").unwrap());
        assert!(t.pane_alive(&crate::tmux::PaneRef::cockpit(&q)));
        assert!(pane_ids(t).contains("tail"));
        // no session at all: nothing to replace
        t.kill_server(CK);
        assert!(wait_until(|| !t.has_session(CK, SESSION)));
        assert!(!replace_stale_queue(t, "same").unwrap());
    }

    #[test]
    fn embedded_tmux_config_is_the_cockpits_own() {
        assert!(super::TMUX_CONF.contains("set -g status off"));
        assert!(super::TMUX_CONF.contains("set -g extended-keys on"));
    }
}
