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

    fn finish(r: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
        match r {
            Ok(r) => r.into_json().map_err(|e| e.to_string()),
            Err(ureq::Error::Status(_, r)) => Err(r
                .into_json::<Value>()
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or_else(|| "request refused".into())),
            Err(e) => Err(format!("daemon not running: cockpit daemon start ({e})")),
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        Self::finish(self.agent.get(&format!("{}{path}", self.base)).call())
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        Self::finish(self.agent.post(&format!("{}{path}", self.base)).set("x-tk", "1").send_json(body))
    }

    pub fn tool(&self, name: &str, args: Value) -> Result<Value, String> {
        self.post(&format!("/api/tools/{name}"), args)
    }

    /// Types into a session's pane (`{"text"}` or `{"keys"}`); refusals carry the cockpit's reason.
    pub fn send(&self, session: &str, body: Value) -> Result<(), String> {
        self.post(&format!("/api/pane/{session}/send"), body).map(|_| ())
    }
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
}
