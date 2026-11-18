//! The TUI talks to the daemon over HTTP only. Refusals come back as 400 `{"error"}`; they are returned as
//! the `Err` text so the caller can show them as the flash message.
use serde_json::Value;
use std::time::Duration;

pub const DEFAULT_URL: &str = "http://127.0.0.1:4747";

pub fn base_url() -> String {
    std::env::var("COCKPIT_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.into())
        .trim_end_matches('/')
        .to_string()
}

#[derive(Clone)]
pub struct Api {
    base: String,
    agent: ureq::Agent,
}

impl Api {
    pub fn new(base: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(500))
            .timeout(Duration::from_secs(5))
            .build();
        Api { base: base.trim_end_matches('/').to_string(), agent }
    }

    pub fn from_env() -> Self {
        Api::new(&base_url())
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// `what` names the tool or path asked for. A 404 on a tool, or without an error text, means an older daemon.
    fn finish(r: Result<ureq::Response, ureq::Error>, what: &str, is_tool: bool) -> Result<Value, String> {
        match r {
            Ok(r) => r.into_json().map_err(|e| e.to_string()),
            Err(ureq::Error::Status(code, r)) => {
                let msg = r.into_json::<Value>().ok().and_then(|v| v["error"].as_str().map(String::from));
                match msg {
                    _ if code == 404 && (is_tool || msg.is_none()) => Err(unknown(what)),
                    Some(m) => Err(m),
                    None => Err("request refused".into()),
                }
            }
            Err(e) => Err(format!("daemon not running: cockpit daemon start ({e})")),
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        Self::finish(self.agent.get(&format!("{}{path}", self.base)).call(), path, false)
    }

    fn post(&self, path: &str, body: Value, is_tool: bool) -> Result<Value, String> {
        let r = self.agent.post(&format!("{}{path}", self.base)).set("x-tk", "1").send_json(body);
        Self::finish(r, if is_tool { path.rsplit('/').next().unwrap_or(path) } else { path }, is_tool)
    }

    pub fn tool(&self, name: &str, args: Value) -> Result<Value, String> {
        self.post(&format!("/api/tools/{name}"), args, true)
    }

    /// Types into a session's pane (`{"text"}` or `{"keys"}`); refusals carry the cockpit's reason.
    pub fn send(&self, session: &str, body: Value) -> Result<(), String> {
        self.post(&format!("/api/pane/{session}/send"), body, false).map(|_| ())
    }
}

fn unknown(what: &str) -> String {
    format!("the daemon doesn't know {what}: restart it (cockpit daemon restart)")
}

/// True when the error means the daemon could not be reached at all.
pub fn is_down(e: &str) -> bool {
    e.starts_with("daemon not running")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreachable_daemon_gives_the_start_hint() {
        let e = Api::new("http://127.0.0.1:1").get("/api/state").unwrap_err();
        assert!(is_down(&e), "{e}");
        assert!(e.contains("cockpit daemon start"));
    }

    /// Answers each request with the next canned `(status line, body)`.
    fn canned(responses: Vec<(&'static str, &'static str)>) -> Api {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for ((status, body), s) in responses.into_iter().zip(l.incoming().flatten()) {
                let mut s = s;
                let _ = s.read(&mut [0u8; 2048]);
                let _ =
                    write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Api::new(&url)
    }

    #[test]
    fn a_404_never_shows_a_status_but_names_what_the_daemon_lacks() {
        let api = canned(vec![
            ("404 Not Found", r#"{"error":"no tool ticket.start"}"#),
            ("404 Not Found", ""),
            ("404 Not Found", r#"{"error":"no live pane for s1"}"#),
            ("400 Bad Request", r#"{"error":"TIC-1 has no project"}"#),
            ("500 Internal Server Error", ""),
        ]);
        let hint = "restart it (cockpit daemon restart)";
        assert_eq!(
            api.tool("ticket.start", serde_json::json!({})).unwrap_err(),
            format!("the daemon doesn't know ticket.start: {hint}")
        );
        assert_eq!(api.get("/api/windows").unwrap_err(), format!("the daemon doesn't know /api/windows: {hint}"));
        // a 404 that carries the daemon's own reason keeps it
        assert_eq!(api.get("/api/pane/s1").unwrap_err(), "no live pane for s1");
        assert_eq!(api.tool("ticket.get", serde_json::json!({})).unwrap_err(), "TIC-1 has no project");
        assert_eq!(api.get("/api/state").unwrap_err(), "request refused");
    }
}
