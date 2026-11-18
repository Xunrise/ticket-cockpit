//! The only place that runs tmux. Both servers are always addressed with an explicit `-L`:
//! the daemon may itself run inside the cockpit's tmux, where a bare `tmux` would pick that server.
//! Pane refs: `cockpit:%N` lives on the cockpit's server, a bare `%N` on the user's default one.
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const SESSION: &str = "cockpit";
/// Name of the home window that holds the queue.
pub const HOME_WINDOW: &str = "cockpit";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Server {
    Cockpit,
    Default,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PaneRef {
    pub server: Server,
    pub id: String,
}

impl PaneRef {
    pub fn parse(s: &str) -> Result<Self> {
        let (server, id) = match s.strip_prefix("cockpit:") {
            Some(id) => (Server::Cockpit, id),
            None => (Server::Default, s),
        };
        if !id.starts_with('%') || id.len() < 2 || !id[1..].chars().all(|c| c.is_ascii_digit()) {
            bail!("bad pane ref {s}");
        }
        Ok(PaneRef { server, id: id.into() })
    }

    pub fn cockpit(id: &str) -> Self {
        PaneRef { server: Server::Cockpit, id: id.into() }
    }
}

impl fmt::Display for PaneRef {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.server {
            Server::Cockpit => write!(f, "cockpit:{}", self.id),
            Server::Default => write!(f, "{}", self.id),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Tmux {
    pub cockpit_sock: String,
    pub default_sock: String,
    /// tmux config for the cockpit's server (read only when the server starts).
    pub conf: Option<PathBuf>,
}

impl Tmux {
    pub fn system() -> Self {
        let conf = crate::paths::tmux_conf();
        Tmux { cockpit_sock: "cockpit".into(), default_sock: "default".into(), conf: conf.exists().then_some(conf) }
    }

    /// Two private sockets for tests, so nothing touches the user's servers.
    pub fn private(tag: &str) -> Self {
        Tmux {
            cockpit_sock: format!("cockpit-test-{tag}"),
            default_sock: format!("cockpit-test-{tag}-default"),
            conf: None,
        }
    }

    pub fn sock(&self, s: Server) -> &str {
        match s {
            Server::Cockpit => &self.cockpit_sock,
            Server::Default => &self.default_sock,
        }
    }

    fn command(&self, s: Server, args: &[&str]) -> Command {
        let mut c = Command::new("tmux");
        c.args(["-L", self.sock(s)]);
        if let (Server::Cockpit, Some(conf)) = (s, &self.conf) {
            c.arg("-f").arg(conf);
        }
        c.args(args);
        c
    }

    fn exec(&self, mut c: Command, input: Option<&str>) -> Result<String> {
        c.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = c.spawn().map_err(|e| anyhow!("tmux: {e}"))?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(text.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("tmux: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs a tmux command; returns stdout without the trailing newline.
    pub fn run(&self, s: Server, args: &[&str]) -> Result<String> {
        Ok(self.exec(self.command(s, args), None)?.trim_end_matches('\n').to_string())
    }

    /// Like `run` with an environment for the spawned tmux (a new server inherits it).
    pub fn run_env(&self, s: Server, args: &[&str], env: Vec<(String, String)>) -> Result<String> {
        let mut c = self.command(s, args);
        c.env_clear().envs(env);
        Ok(self.exec(c, None)?.trim_end_matches('\n').to_string())
    }

    pub fn quiet(&self, s: Server, args: &[&str]) -> String {
        self.run(s, args).unwrap_or_default()
    }

    pub fn has_session(&self, s: Server, name: &str) -> bool {
        self.run(s, &["has-session", "-t", &format!("={name}")]).is_ok()
    }

    /// Every pane on both servers: ref -> `session:window`.
    pub fn list_panes(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for s in [Server::Default, Server::Cockpit] {
            for l in self.quiet(s, &["list-panes", "-a", "-F", "#{pane_id} #{session_name}:#{window_name}"]).lines() {
                if let Some((id, w)) = l.split_once(' ') {
                    m.insert(PaneRef { server: s, id: id.into() }.to_string(), w.to_string());
                }
            }
        }
        m
    }

    pub fn pane_alive(&self, p: &PaneRef) -> bool {
        self.quiet(p.server, &["list-panes", "-a", "-F", "#{pane_id}"]).lines().any(|l| l == p.id)
    }

    /// Screen text; with `escapes` the SGR codes are kept (needed to tell ghost text from a draft).
    pub fn capture(&self, p: &PaneRef, escapes: bool, scrollback: Option<u32>) -> Result<String> {
        let back = scrollback.map(|n| format!("-{n}"));
        let mut a = vec!["capture-pane", "-p"];
        if escapes {
            a.push("-e");
        }
        a.extend(["-t", &p.id]);
        if let Some(b) = &back {
            a.extend(["-S", b]);
        }
        self.exec(self.command(p.server, &a), None)
    }

    pub fn send_keys(&self, p: &PaneRef, keys: &[&str]) -> Result<()> {
        let mut a = vec!["send-keys", "-t", &p.id];
        a.extend(keys);
        self.run(p.server, &a).map(|_| ())
    }

    pub fn send_literal(&self, p: &PaneRef, text: &str) -> Result<()> {
        self.run(p.server, &["send-keys", "-t", &p.id, "-l", text]).map(|_| ())
    }

    pub fn set_pane_option(&self, p: &PaneRef, key: &str, value: &str) -> Result<()> {
        self.run(p.server, &["set", "-p", "-t", &p.id, key, value]).map(|_| ())
    }

    /// Bracketed paste into a pane without pressing Enter.
    pub fn paste(&self, p: &PaneRef, text: &str) -> Result<()> {
        self.exec(self.command(p.server, &["load-buffer", "-b", "cockpit", "-"]), Some(text))?;
        self.run(p.server, &["paste-buffer", "-p", "-d", "-b", "cockpit", "-t", &p.id]).map(|_| ())
    }

    pub fn kill_server(&self, s: Server) {
        let _ = self.run(s, &["kill-server"]);
    }
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A private tmux pair that is killed on drop. `None` when tmux is missing.
    pub struct Guard(pub Tmux);

    pub fn guard() -> Option<Guard> {
        Command::new("tmux").arg("-V").output().ok()?;
        let tag = format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
        Some(Guard(Tmux::private(&tag)))
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.kill_server(Server::Cockpit);
            self.0.kill_server(Server::Default);
            // a killed server can leave its socket file behind
            let uid = crate::util::sh("id", &["-u"], None);
            let dir = std::env::var("TMUX_TMPDIR").unwrap_or_else(|_| "/private/tmp".into());
            for s in [&self.0.cockpit_sock, &self.0.default_sock] {
                for base in [dir.clone(), "/tmp".into()] {
                    let _ = std::fs::remove_file(format!("{base}/tmux-{uid}/{s}"));
                }
            }
        }
    }

    pub fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_refs_roundtrip() {
        let c = PaneRef::parse("cockpit:%12").unwrap();
        assert_eq!((c.server, c.id.as_str()), (Server::Cockpit, "%12"));
        assert_eq!(c.to_string(), "cockpit:%12");
        let d = PaneRef::parse("%3").unwrap();
        assert_eq!(d.server, Server::Default);
        assert_eq!(d.to_string(), "%3");
        assert!(PaneRef::parse("3").is_err());
        assert!(PaneRef::parse("cockpit:%x").is_err());
    }

    #[test]
    fn private_sockets_never_use_real_names() {
        let t = Tmux::private("1");
        assert_ne!(t.cockpit_sock, "cockpit");
        assert_ne!(t.default_sock, "default");
    }

    #[test]
    fn list_capture_send_and_options_on_a_private_server() {
        let Some(g) = testutil::guard() else { return };
        let t = &g.0;
        t.run(Server::Cockpit, &["new-session", "-d", "-s", SESSION, "-x", "100", "-y", "30", "cat"]).unwrap();
        let id = t.run(Server::Cockpit, &["list-panes", "-t", SESSION, "-F", "#{pane_id}"]).unwrap();
        let p = PaneRef::cockpit(&id);
        assert!(t.list_panes().contains_key(&p.to_string()));
        assert!(t.pane_alive(&p));
        t.send_literal(&p, "hello cockpit").unwrap();
        assert!(testutil::wait_until(|| t.capture(&p, false, None).unwrap().contains("hello cockpit")));
        t.set_pane_option(&p, "@tk_state", "turn").unwrap();
        assert_eq!(t.run(Server::Cockpit, &["display", "-p", "-t", &p.id, "#{@tk_state}"]).unwrap(), "turn");
        t.paste(&p, " pasted").unwrap();
        assert!(testutil::wait_until(|| t.capture(&p, false, None).unwrap().contains("pasted")));
        assert!(!t.pane_alive(&PaneRef::cockpit("%9999")));
        // the other private server is a different one
        assert!(!t.has_session(Server::Default, SESSION));
    }
}
