//! Updates: what is installed, whether something newer exists (local checkout or GitHub release),
//! and the flows that apply it (`cockpit update | reload | rollback`).
pub mod local;
pub mod release;

use crate::ctx::Ctx;
use crate::tmux::{Server, SESSION};
use crate::{daemon, settings};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The commit this binary was built from; `COCKPIT_GIT_SHA` overrides it (demos, tests).
pub fn installed_commit() -> String {
    std::env::var("COCKPIT_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("COCKPIT_GIT_SHA").to_string())
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `0.2.0 (abc123def456)`.
pub fn installed() -> String {
    format!("{} ({})", version(), installed_commit())
}

pub(crate) fn setting(ctx: &Ctx, key: &str) -> String {
    ctx.db()
        .ok()
        .and_then(|c| settings::get(&c, key).ok())
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// What an update would bring, per channel. The daemon's periodic check is disabled by `check_updates`.
pub fn check(ctx: &Ctx) -> Result<Value> {
    compute(ctx, true)
}

fn compute(ctx: &Ctx, respect_setting: bool) -> Result<Value> {
    let conn = ctx.db()?;
    let channel = setting(ctx, "update_channel");
    let installed_commit = installed_commit();
    let mut st = if respect_setting && !settings::get_bool(&conn, "check_updates") {
        json!({"available": null, "commits_ahead": 0, "behind_remote": 0, "summary": "update checks are off", "notes": ""})
    } else if channel == "release" {
        release::status(&release::base(), &setting(ctx, "release_repo"), version(), &installed_commit)
    } else {
        local::status(&setting(ctx, "cockpit_repo"), &installed_commit)
    };
    st["channel"] = channel.into();
    st["installed"] = installed().into();
    st["installed_commit"] = installed_commit.into();
    st["installed_version"] = version().into();
    st["checked_at"] = crate::util::now_iso().into();
    Ok(st)
}

/// Stores a fresh check for `cockpit.update_status` and the summary. Never fails: a failed check keeps the old one.
pub fn refresh(ctx: &Ctx) {
    match check(ctx) {
        Ok(st) => {
            if let Ok(mut g) = ctx.update.lock() {
                *g = st;
            }
        }
        Err(e) => crate::errors::log_quiet(ctx, "daemon", &format!("update check: {e:#}"), ""),
    }
}

/// The cached check (computed on first use, or when `force`).
pub fn status(ctx: &Ctx, force: bool) -> Result<Value> {
    let cached = ctx.update.lock().map(|g| g.clone()).unwrap_or(Value::Null);
    if force || cached.is_null() {
        refresh(ctx);
    }
    Ok(ctx.update.lock().map(|g| g.clone()).unwrap_or(Value::Null))
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| "cockpit".into())
}

pub fn reload_steps() -> Vec<String> {
    vec![
        format!("restart the daemon: launchctl kickstart -k {}", daemon::target()),
        "replace the queue with the installed binary: exec cockpit".into(),
    ]
}

pub fn update_steps(ctx: &Ctx) -> Vec<String> {
    let mut s = if setting(ctx, "update_channel") == "release" {
        release::steps(&release::base(), &setting(ctx, "release_repo"), &exe())
    } else {
        local::steps(&setting(ctx, "cockpit_repo"))
    };
    s.extend(reload_steps());
    s
}

/// The shell line a window runs: the command, then a clear last line and a wait for a key, so the window
/// never vanishes before it is read. (A successful update execs into the new queue instead of getting here.)
pub fn window_script(exe: &str, sub: &str) -> String {
    format!(
        "{} {sub}; rc=$?; echo; [ $rc -ne 0 ] && echo \"{sub} failed (status $rc)\"; printf 'press any key to close '; read -k1 -s",
        quote(exe)
    )
}

/// Opens (or recreates) a visible window in the cockpit session running `cockpit <sub>`, and selects it.
/// The window gets the cockpit's own environment, so it reports the state the queue showed.
pub fn open_window(ctx: &Ctx, name: &str, sub: &str) -> Result<Value> {
    let tmux = &ctx.tmux;
    let target = format!("={SESSION}:{name}");
    if crate::panes::window_exists(tmux, name) {
        let _ = tmux.run(Server::Cockpit, &["kill-window", "-t", &target]);
    }
    let cmd = format!("/bin/zsh -lc {}", quote(&window_script(&ctx.exe, sub)));
    let vars: Vec<(&str, String)> =
        ["COCKPIT_DB", "COCKPIT_URL", "COCKPIT_GIT_SHA", "COCKPIT_RELEASE_BASE", "COCKPIT_ALLOW_REAL_UPDATE"]
            .iter()
            .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (*k, v)))
            .chain([("COCKPIT_TMUX_SOCKET", tmux.cockpit_sock.clone())])
            .collect();
    let env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let home = crate::paths::home().to_string_lossy().into_owned();
    crate::panes::cockpit_window(tmux, name, &home, &env, &cmd)?;
    let _ = tmux.run(Server::Cockpit, &["select-window", "-t", &target]);
    Ok(json!({"window": format!("cockpit:{name}")}))
}

/// Where `cockpit install` recorded the binary (the launchd agent's program).
fn recorded_exe(plist: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(plist).ok()?;
    text.split("<string>")
        .skip(1)
        .filter_map(|p| p.split("</string>").next())
        .find(|p| p.ends_with("/cockpit"))
        .map(PathBuf::from)
}

/// `update` and `rollback` replace the installed binary, so they only run from it: a development build
/// (target/debug, a worktree) refuses.
pub fn check_installed(exe: &Path, home: &Path, recorded: Option<&Path>, channel: &str) -> Result<()> {
    let default = home.join(if channel == "release" { ".local/bin/cockpit" } else { ".cargo/bin/cockpit" });
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let e = canon(exe);
    let known = [home.join(".cargo/bin/cockpit"), home.join(".local/bin/cockpit")];
    if known.iter().map(|p| canon(p)).chain(recorded.map(canon)).any(|p| p == e) {
        return Ok(());
    }
    bail!(
        "this is a development build ({}); update applies to the installed cockpit at {}",
        exe.display(),
        recorded.unwrap_or(&default).display()
    )
}

fn guard(ctx: &Ctx) -> Result<()> {
    let recorded = recorded_exe(&crate::paths::launch_agent_plist());
    check_installed(&exe(), &crate::paths::home(), recorded.as_deref(), &setting(ctx, "update_channel"))
}

/// An instance on its own database (`COCKPIT_DB`, e.g. a prototype) must never restart the real daemon:
/// it only lists the steps, unless `COCKPIT_ALLOW_REAL_UPDATE` is set.
fn isolated() -> bool {
    std::env::var_os("COCKPIT_DB").is_some() && std::env::var_os("COCKPIT_ALLOW_REAL_UPDATE").is_none()
}

fn print_steps(steps: &[String]) {
    for (i, s) in steps.iter().enumerate() {
        println!("{}. {s}", i + 1);
    }
}

fn restart_daemon() {
    if !daemon::loaded() {
        println!("daemon agent not loaded: nothing to restart");
        return;
    }
    let _ = daemon::launchctl(&["kickstart", "-k", &daemon::target()]);
    let port = std::env::var("COCKPIT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(daemon::DEFAULT_PORT);
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..50 {
        if crate::util::http_get(&addr, "/api/health", std::time::Duration::from_millis(500)).is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Restarts the daemon and replaces this process with the installed binary (a new queue). Returns only on failure.
fn reload_now() -> Result<()> {
    println!("restarting the daemon");
    restart_daemon();
    println!("replacing the queue");
    let err = Command::new(exe()).exec();
    bail!("cannot start {}: {err}", exe().display())
}

pub fn reload_command(mut args: Vec<String>) -> Result<()> {
    let mut dry = crate::cli::switch(&mut args, "dry-run");
    if isolated() && !dry {
        println!("isolated instance (COCKPIT_DB is set): not doing it, the steps would be:");
        dry = true;
    }
    if dry {
        print_steps(&reload_steps());
        return Ok(());
    }
    reload_now()
}

fn run_sh(script: &str) -> Result<()> {
    let st = Command::new("/bin/sh").args(["-c", script]).status()?;
    if !st.success() {
        bail!("step failed ({st})");
    }
    Ok(())
}

pub fn update_command(mut args: Vec<String>) -> Result<()> {
    let ctx = Ctx::from_env();
    let stage = crate::cli::flag(&mut args, "stage");
    if crate::cli::switch(&mut args, "dry-run") {
        print_steps(&update_steps(&ctx));
        return Ok(());
    }
    let repo = setting(&ctx, "release_repo");
    if let Some(dir) = stage {
        // download, verify and unpack only (demos, tests): installs nothing
        let (tag, bin) = release::stage(&release::base(), &repo, Path::new(&dir))?;
        println!("sha256 ok\nstaged {tag}: {}", bin.display());
        return Ok(());
    }
    guard(&ctx)?;
    let st = compute(&ctx, false)?; // an explicit update checks even when the periodic check is off
    let release = setting(&ctx, "update_channel") == "release";
    if st["available"].is_null() {
        let theirs = if release {
            format!("the latest release is {}", st["latest"].as_str().unwrap_or("unknown"))
        } else {
            format!("main is {}", st["head"].as_str().unwrap_or("unknown"))
        };
        let mine = if release { installed() } else { installed_commit() };
        println!("nothing to update: you run {mine}, {theirs}");
        return Ok(());
    }
    if st["behind_remote"].as_i64().unwrap_or(0) > 0 {
        println!("note: origin/main has commits main does not: {}", st["summary"].as_str().unwrap_or(""));
    }
    if release {
        let tmp = std::env::temp_dir().join(format!("cockpit-update-{}", std::process::id()));
        let (tag, bin) = release::stage(&release::base(), &repo, &tmp)?;
        println!("sha256 ok");
        release::swap_in(&bin, &exe())?;
        let _ = std::fs::remove_dir_all(&tmp);
        println!("installed {tag}; the previous binary is cockpit.prev (cockpit rollback)");
    } else {
        run_sh(&local::install_script(&setting(&ctx, "cockpit_repo"), &exe().to_string_lossy()))?;
    }
    println!("update done: reloading…");
    reload_now()
}

pub fn rollback_command(mut args: Vec<String>) -> Result<()> {
    let e = exe();
    if crate::cli::switch(&mut args, "dry-run") {
        println!("1. swap {} and {}", e.display(), release::prev_of(&e).display());
        print_steps(&reload_steps());
        return Ok(());
    }
    guard(&Ctx::from_env())?;
    release::rollback(&e)?;
    println!("rolled back");
    reload_now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::tmux::testutil::{guard, wait_until};
    use crate::tmux::{Server, Tmux};

    #[test]
    fn update_status_is_checked_once_then_cached_and_summarised() {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("update-unit"));
        let conn = c.db().unwrap();
        settings::set(&conn, "cockpit_repo", &json!(d.path().join("none").to_string_lossy())).unwrap();
        let s = crate::tools::call(&c, "cockpit.update_status", json!({})).unwrap();
        assert_eq!(s["channel"], "local");
        assert!(s["summary"].as_str().unwrap().contains("not a git checkout"));
        assert_eq!(s["installed"], installed().as_str());
        // cached: a changed setting shows only on refresh
        settings::set(&conn, "check_updates", &json!(false)).unwrap();
        assert!(status(&c, false).unwrap()["summary"].as_str().unwrap().contains("not a git checkout"));
        assert_eq!(status(&c, true).unwrap()["summary"], "update checks are off");
        let sum = crate::summary::summary(&c).unwrap();
        assert_eq!(sum["update"], json!({"available": false, "commits": 0}));
    }

    #[test]
    fn the_summary_reports_waiting_commits() {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("update-unit2"));
        *c.update.lock().unwrap() = json!({"available": "abc", "commits_ahead": 3});
        assert_eq!(crate::summary::summary(&c).unwrap()["update"], json!({"available": true, "commits": 3}));
    }

    #[test]
    fn only_the_installed_binary_may_update() {
        let home = Path::new("/h");
        let dev = Path::new("/w/target/debug/cockpit");
        let e = check_installed(dev, home, None, "local").unwrap_err().to_string();
        assert!(
            e.contains("development build (/w/target/debug/cockpit)") && e.contains("/h/.cargo/bin/cockpit"),
            "{e}"
        );
        assert!(check_installed(dev, home, None, "release").unwrap_err().to_string().contains("/h/.local/bin/cockpit"));
        assert!(check_installed(Path::new("/h/.cargo/bin/cockpit"), home, None, "local").is_ok());
        assert!(check_installed(Path::new("/h/.local/bin/cockpit"), home, None, "release").is_ok());
        assert!(check_installed(Path::new("/opt/c/cockpit"), home, Some(Path::new("/opt/c/cockpit")), "local").is_ok());
        let e = check_installed(dev, home, Some(Path::new("/opt/c/cockpit")), "local").unwrap_err().to_string();
        assert!(e.contains("installed cockpit at /opt/c/cockpit"), "{e}");
    }

    #[test]
    fn the_recorded_binary_is_read_from_the_launch_agent() {
        let d = Dir::new();
        let plist = d.path().join("a.plist");
        std::fs::write(&plist, "<array><string>/opt/c/cockpit</string><string>daemon</string></array>").unwrap();
        assert_eq!(recorded_exe(&plist), Some(PathBuf::from("/opt/c/cockpit")));
        assert_eq!(recorded_exe(&d.path().join("none")), None);
    }

    #[test]
    fn dry_run_steps_per_channel_end_with_the_reload() {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("update-steps"));
        let conn = c.db().unwrap();
        settings::set(&conn, "cockpit_repo", &json!("/src/cockpit")).unwrap();
        let local = update_steps(&c);
        assert_eq!((local[0].as_str(), local[2].as_str()), ("cd /src/cockpit", "cargo install --path ."));
        settings::set(&conn, "update_channel", &json!("release")).unwrap();
        let rel = update_steps(&c);
        assert!(
            rel[0].starts_with("curl ")
                && rel[0].ends_with("/Xunrise/ticket-cockpit/releases/latest/download/manifest.json"),
            "{rel:?}"
        );
        for steps in [&local, &rel] {
            assert!(steps.last().unwrap().starts_with("replace the queue"));
            assert!(steps[steps.len() - 2].starts_with("restart the daemon"));
        }
        let t = crate::tools::call(&c, "cockpit.reload", json!({"dry_run": true})).unwrap();
        assert_eq!(t["steps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_window_script_ends_with_a_last_line_and_waits_for_a_key() {
        let s = window_script("/x/cockpit", "update");
        assert_eq!(
            s,
            "'/x/cockpit' update; rc=$?; echo; [ $rc -ne 0 ] && echo \"update failed (status $rc)\"; printf 'press any key to close '; read -k1 -s"
        );
        assert!(window_script("/it's/cockpit", "reload").starts_with("'/it'\\''s/cockpit' reload;"));
    }

    fn stand_in(d: &Dir, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = d.path().join("fake-cockpit");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn screen(t: &Tmux, name: &str) -> String {
        t.quiet(Server::Cockpit, &["capture-pane", "-p", "-t", &format!("={SESSION}:{name}")])
    }

    #[test]
    fn the_update_window_is_selected_and_survives_a_clean_exit() {
        let Some(g) = guard() else { return };
        let d = Dir::new();
        let mut c = Ctx::for_test(d.path(), g.0.clone());
        crate::panes::cockpit_window(&c.tmux, crate::tmux::HOME_WINDOW, "/tmp", &[], "sleep 600").unwrap();
        c.exe = stand_in(&d, "echo 'nothing to update: you run abc, main is abc'; exit 0");
        let r = open_window(&c, "cockpit-update", "update").unwrap();
        assert_eq!(r["window"], "cockpit:cockpit-update");
        let active =
            || c.tmux.quiet(Server::Cockpit, &["display", "-p", "-t", &format!("={SESSION}:"), "#{window_name}"]);
        assert_eq!(active(), "cockpit-update", "selected");
        assert!(wait_until(|| screen(&c.tmux, "cockpit-update").contains("press any key to close")));
        let s = screen(&c.tmux, "cockpit-update");
        assert!(s.contains("nothing to update: you run abc, main is abc"), "{s}");
        assert!(!s.contains("failed"), "{s}");
        assert!(crate::panes::window_exists(&c.tmux, "cockpit-update"), "a 0 exit does not close the window");
        // any key closes it
        c.tmux.run(Server::Cockpit, &["send-keys", "-t", &format!("={SESSION}:cockpit-update"), "x"]).unwrap();
        assert!(wait_until(|| !crate::panes::window_exists(&c.tmux, "cockpit-update")));
    }

    #[test]
    fn a_failing_window_names_the_status() {
        let Some(g) = guard() else { return };
        let d = Dir::new();
        let mut c = Ctx::for_test(d.path(), g.0.clone());
        c.exe = stand_in(&d, "echo 'cockpit: step failed' >&2; exit 3");
        open_window(&c, "cockpit-reload", "reload").unwrap();
        assert!(wait_until(|| screen(&c.tmux, "cockpit-reload").contains("press any key to close")));
        let s = screen(&c.tmux, "cockpit-reload");
        assert!(s.contains("cockpit: step failed") && s.contains("reload failed (status 3)"), "{s}");
    }

    #[test]
    fn the_window_inherits_the_cockpits_environment() {
        let Some(g) = guard() else { return };
        let d = Dir::new();
        let mut c = Ctx::for_test(d.path(), g.0.clone());
        c.exe = stand_in(&d, "echo \"sock=$COCKPIT_TMUX_SOCKET\"; exit 0");
        open_window(&c, "cockpit-update", "update").unwrap();
        assert!(wait_until(|| screen(&c.tmux, "cockpit-update").contains("sock=")));
        assert!(screen(&c.tmux, "cockpit-update").contains(&format!("sock={}", c.tmux.cockpit_sock)));
    }
}
