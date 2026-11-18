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
use crate::tmux::{Server, Tmux, SESSION};
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

/// `cockpit` with no arguments: outside the cockpit's tmux, attach to (or create) the session; inside, run the queue.
pub fn launch() -> Result<()> {
    let conf = write_tmux_conf()?;
    let tmux = Tmux::system();
    if HookEnv::from_process().on_cockpit(&tmux) {
        return run_queue(tmux);
    }
    let exe = std::env::current_exe()?;
    // the session can outlive the queue (ticket windows keep running): bring the queue back first
    let t = Tmux { conf: Some(conf.clone()), ..tmux.clone() };
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
    #[test]
    fn embedded_tmux_config_is_the_cockpits_own() {
        assert!(super::TMUX_CONF.contains("set -g status off"));
        assert!(super::TMUX_CONF.contains("set -g extended-keys on"));
    }
}
