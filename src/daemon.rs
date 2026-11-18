//! The background service: `daemon run` serves the HTTP API, the rest manages the launchd agent.
use crate::cli::{flag, switch};
use crate::ctx::Ctx;
use crate::http::{self, AppState};
use crate::paths;
use crate::util::{http_get, sh};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub const LABEL: &str = "io.cockpit.daemon";
pub const DEFAULT_PORT: u16 = 4747;

/// Background work on one thread: finishes requested close-outs every 2 s.
fn scheduler(ctx: Ctx) {
    loop {
        if let Err(e) = crate::closeout::finish_requested(&ctx) {
            crate::errors::log_quiet(&ctx, "daemon", &format!("close-out: {e:#}"), "");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn run(port: u16) -> Result<()> {
    let state = AppState::new(Ctx::from_env());
    let ctx = state.ctx.clone();
    std::thread::spawn(move || scheduler(ctx));
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        let bound = listener.local_addr()?.port();
        state.port.store(bound, std::sync::atomic::Ordering::Relaxed);
        println!("cockpit daemon listening on http://localhost:{bound} (db {})", state.ctx.db_path.display());
        http::serve(state, listener).await
    })
}

pub fn domain() -> String {
    format!("gui/{}", sh("id", &["-u"], None))
}

pub fn target() -> String {
    format!("{}/{LABEL}", domain())
}

pub fn launchctl(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("launchctl").args(args).output().map_err(|e| anyhow!("launchctl: {e}"))?;
    if !out.status.success() {
        bail!("launchctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `pid = N` line of `launchctl print`.
pub fn parse_pid(print_output: &str) -> Option<u32> {
    print_output.lines().find_map(|l| l.trim().strip_prefix("pid = ")?.trim().parse().ok())
}

pub fn format_uptime(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

fn health(port: u16) -> Option<Value> {
    serde_json::from_str(&http_get(&format!("127.0.0.1:{port}"), "/api/health", Duration::from_millis(800))?).ok()
}

/// The build id of the daemon answering on `port` ("unknown" for one that predates build ids); `None` when none runs.
pub fn running_build(port: u16) -> Option<String> {
    health(port).map(|h| h["build"].as_str().unwrap_or("unknown").to_string())
}

fn installed() -> bool {
    paths::launch_agent_plist().exists()
}

pub fn loaded() -> bool {
    launchctl(&["print", &target()]).is_ok()
}

fn install_hint() -> String {
    format!(
        "The launchd agent {LABEL} is not installed ({} is missing).\nSet it up with `cockpit install`, or run the daemon in the foreground: `cockpit daemon run`.",
        paths::launch_agent_plist().display()
    )
}

fn wait_for(port: u16, up: bool) -> bool {
    for _ in 0..50 {
        if health(port).is_some() == up {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

pub fn status(port: u16) -> Value {
    let agent_pid = launchctl(&["print", &target()]).ok().and_then(|o| parse_pid(&o));
    match health(port) {
        Some(h) => {
            json!({"running": true, "pid": h["pid"], "port": h["port"], "db": h["db"], "uptime_s": h["uptime_s"], "started": h["started"], "build": h["build"], "agent_installed": installed()})
        }
        None => {
            json!({"running": false, "pid": agent_pid, "port": port, "db": paths::db_path(), "uptime_s": null, "agent_installed": installed()})
        }
    }
}

fn print_status(s: &Value, as_json: bool) {
    if as_json {
        println!("{s}");
        return;
    }
    if s["running"] == true {
        println!("cockpit daemon: running");
        println!("pid      {}", s["pid"]);
        println!("port     {}", s["port"]);
        println!("db       {}", s["db"].as_str().unwrap_or(""));
        println!("uptime   {}", format_uptime(s["uptime_s"].as_i64().unwrap_or(0)));
        println!("build    {}", s["build"].as_str().unwrap_or("unknown (older than build ids)"));
    } else {
        println!("cockpit daemon: not running (nothing answers on port {})", s["port"]);
        println!("db       {}", s["db"].as_str().unwrap_or(""));
        if s["agent_installed"] != true {
            println!("{}", install_hint());
        } else {
            println!("start it with `cockpit daemon start`");
        }
    }
}

/// `daemon run|status|start|stop|restart|logs`.
pub fn command(mut args: Vec<String>) -> Result<()> {
    let sub = if args.is_empty() { "status".to_string() } else { args.remove(0) };
    let port: u16 = flag(&mut args, "port").map(|p| p.parse()).transpose()?.unwrap_or(DEFAULT_PORT);
    match sub.as_str() {
        "run" => run(port),
        "status" => {
            let s = status(port);
            print_status(&s, switch(&mut args, "json"));
            if s["running"] == true {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        "start" => start(port),
        "stop" => stop(port),
        "restart" => {
            if loaded() {
                launchctl(&["kickstart", "-k", &target()])?;
                wait_for(port, true);
                print_status(&status(port), false);
                Ok(())
            } else {
                start(port)
            }
        }
        "logs" => logs(args),
        other => bail!("unknown daemon command {other} (run|status|start|stop|restart|logs)"),
    }
}

fn start(port: u16) -> Result<()> {
    if health(port).is_some() {
        println!("cockpit daemon: already running");
        return Ok(());
    }
    if !installed() && !loaded() {
        bail!("{}", install_hint());
    }
    if loaded() {
        launchctl(&["kickstart", &target()])?;
    } else {
        launchctl(&["bootstrap", &domain(), &paths::launch_agent_plist().to_string_lossy()])?;
    }
    if !wait_for(port, true) {
        bail!("started the agent, but nothing answers on port {port}; see `cockpit daemon logs`");
    }
    print_status(&status(port), false);
    Ok(())
}

fn stop(port: u16) -> Result<()> {
    if !loaded() {
        if installed() {
            println!("cockpit daemon: not loaded, nothing to stop");
        } else {
            println!("cockpit daemon: nothing to stop\n{}", install_hint());
        }
        return Ok(());
    }
    launchctl(&["bootout", &target()])?;
    if !wait_for(port, false) {
        bail!("asked launchd to stop the agent, but port {port} still answers");
    }
    println!("cockpit daemon: stopped (`cockpit daemon start` brings it back)");
    Ok(())
}

fn logs(mut args: Vec<String>) -> Result<()> {
    let follow = switch(&mut args, "follow") || switch(&mut args, "f");
    let n: usize = flag(&mut args, "n").map(|n| n.parse()).transpose()?.unwrap_or(40);
    let file = paths::log_dir().join("daemon.log");
    if !file.exists() {
        bail!("no log yet at {} (the launchd agent writes it)", file.display());
    }
    if follow {
        let st = std::process::Command::new("tail").args(["-n", &n.to_string(), "-f"]).arg(&file).status()?;
        std::process::exit(st.code().unwrap_or(0));
    }
    let text = std::fs::read_to_string(&file)?;
    let lines: Vec<&str> = text.lines().collect();
    println!("{}", lines[lines.len().saturating_sub(n)..].join("\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_is_read_from_launchctl_print() {
        let out = "io.cockpit.daemon = {\n\tactive count = 1\n\tstate = running\n\n\tpid = 4242\n\tlast exit code = (never exited)\n}";
        assert_eq!(parse_pid(out), Some(4242));
        assert_eq!(parse_pid("state = waiting\n\tlast exit code = 1"), None);
    }

    #[test]
    fn uptime_is_human_readable() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(125), "2m 5s");
        assert_eq!(format_uptime(3 * 3600 + 12 * 60 + 7), "3h 12m");
        assert_eq!(format_uptime(2 * 86_400 + 5 * 3600), "2d 5h");
    }

    #[test]
    fn daemon_answers_health_on_an_ephemeral_port() {
        let d = crate::db::tempdir::Dir::new();
        let state = AppState::new(Ctx::for_test(d.path(), crate::tmux::Tmux::private("daemon-unit")));
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0))).unwrap();
        let port = listener.local_addr().unwrap().port();
        state.port.store(port, std::sync::atomic::Ordering::Relaxed);
        rt.spawn(http::serve(state, listener));
        let h = health(port).expect("daemon answers");
        assert_eq!(h["port"], port);
        assert_eq!(h["pid"], std::process::id());
        assert_eq!(h["build"], crate::util::build_id());
        assert_eq!(running_build(port), Some(crate::util::build_id()));
        assert_eq!(status(port)["build"], crate::util::build_id());
        let s = status(port);
        assert_eq!((s["running"].as_bool(), s["port"].as_u64()), (Some(true), Some(port as u64)));
        // the Host header the client sends (localhost:<port>) passes the guard
        assert!(http_get(&format!("localhost:{port}"), "/api/summary", Duration::from_secs(2)).is_some());
    }
}
