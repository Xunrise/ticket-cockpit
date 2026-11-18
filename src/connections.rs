//! What the cockpit is connected to: read-only status plus a help text per entry.
use crate::ctx::Ctx;
use crate::util::http_get;
use serde_json::{json, Value};
use std::time::Duration;

/// `host:port` of the neighbours; overridable for tests.
#[derive(Clone)]
pub struct Endpoints {
    pub cogitator: String,
    pub waypoint: String,
    pub ollama: String,
    pub palaver_sock: std::path::PathBuf,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            cogitator: "127.0.0.1:7777".into(),
            waypoint: "127.0.0.1:5174".into(),
            ollama: "127.0.0.1:11434".into(),
            palaver_sock: crate::paths::palaver_sock(),
        }
    }
}

const HOOK_EVENTS: [&str; 7] =
    ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop", "SessionEnd"];

fn help(name: &str) -> &'static str {
    match name {
        "Claude Code hooks" => "How the cockpit knows what every Claude session is doing, wherever it runs (cockpit, your own tmux, the desktop app).\n\nIn ~/.claude/settings.json, these hook events call \"cockpit hook\":\n  SessionStart       registers the session, claims its ticket, tells Claude about the cockpit\n  UserPromptSubmit   marks it working, records your prompt\n  PreToolUse         records the tool call; an AskUserQuestion marks it \"needs you\"\n  PostToolUse        keeps the trail of recent tool calls\n  Notification       a permission prompt marks it \"needs you\"\n  Stop               your turn; stores Claude's last message\n  SessionEnd         ended\n\nThe hook never blocks a session: errors are swallowed, timeout 5 s, a few milliseconds per call.\n\nPartial means only some of these events are wired; run cockpit install to add the rest.",
        "Claude MCP" => "Planned: the cockpit's tools (tickets, projects, panes, meetings, settings) as an MCP server, so Claude calls them as tools.\n\nUntil then, Claude sessions use the same tools through the CLI: cockpit tool <name> '{json}'. The session start message tells them how.",
        "Cogitator" => "Your desktop layer: the SketchyBar bottom bar, the live wallpaper (the orrery) and Moth, the cat that comments via Ollama.\n\nFrom the cockpit it reads /api/summary every second:\n  bar          sessions waiting on you, longest first; click opens the cockpit on that session\n  wallpaper    rings per project, sessions as bodies (needs pulses, working orbits), ready tickets as moons, today's closed tickets as a constellation\n  cat          gets your current work as context; nudges about scattered focus, breaks, water, forgotten sessions, and tips after calls\n\nNot connected means its daemon on 127.0.0.1:7777 does not answer, or runs the old version that reads its own hook log instead of the cockpit.",
        "Ollama" => "The local language model behind the cat's remarks (llama3.1:8b on localhost:11434). Nothing in the cockpit depends on it; without it the cat stays silent.",
        "Palaver" => "Your meeting recorder. The cockpit talks to Palaver's own MCP server (palaver mcp) to:\n  list projects and meetings for the pickers (l on a ticket)\n  read a meeting's notes and transcript when you link it\n  let a Claude session search the meetings of a project (ctrl-a in the meeting picker)\n\nThe speaking-coach baseline is read from ~/.config/palaver/coach-baseline.json for the cat's after-call tips.",
        "Waypoint" => "Waypoint turns a code change into a review case. The cockpit already offers it an API:\n  ticket.for_change   find the ticket behind a session, a repo + branch, or a key in the branch name\n  ticket.link_case    link a case to a ticket\n  session.get         a session's repo, branch, state and tmux window\n  /api/stream         live updates\n\nPartial means Waypoint runs, but its side of the integration is not built yet.",
        "Jira" => "Placeholder. Later: link a cockpit project to a Jira project and keep tickets in sync (create, status, comments), with the cockpit staying the local source of what sessions work on.",
        "GitHub Issues" => "Placeholder. Later: link a cockpit project to a GitHub repo's issues, same idea as Jira.",
        _ => "",
    }
}

/// Which of the hook events call `cockpit hook` in Claude's settings.
pub fn wired_hook_events(settings_json: &str) -> Vec<&'static str> {
    let Ok(v) = serde_json::from_str::<Value>(settings_json) else { return vec![] };
    HOOK_EVENTS
        .iter()
        .copied()
        .filter(|ev| {
            v["hooks"][ev].as_array().is_some_and(|groups| {
                groups.iter().flat_map(|g| g["hooks"].as_array().into_iter().flatten()).any(|h| {
                    let mut words = h["command"].as_str().unwrap_or("").split_whitespace().rev();
                    words.next() == Some("hook")
                        && words.next().is_some_and(|p| p.rsplit('/').next() == Some("cockpit"))
                })
            })
        })
        .collect()
}

pub fn status(ctx: &Ctx) -> Value {
    status_with(ctx, &Endpoints::default())
}

pub fn status_with(ctx: &Ctx, ep: &Endpoints) -> Value {
    let t = Duration::from_millis(600);
    let (cog, wp, ollama) = std::thread::scope(|s| {
        let a = s.spawn(|| http_get(&ep.cogitator, "/state.json", t));
        let b = s.spawn(|| http_get(&ep.waypoint, "/api/changes", t));
        let c = s.spawn(|| http_get(&ep.ollama, "/api/tags", t));
        (a.join().ok().flatten(), b.join().ok().flatten(), c.join().ok().flatten())
    });
    let wired = wired_hook_events(&std::fs::read_to_string(&ctx.claude_settings).unwrap_or_default()).len();
    let cog_reads_cockpit = cog
        .as_deref()
        .and_then(|c| serde_json::from_str::<Value>(c).ok())
        .is_some_and(|v| !matches!(v["cockpit"], Value::Null | Value::Bool(false)));
    let rows = [
        (
            "Claude Code hooks",
            match wired {
                0 => "off",
                n if n == HOOK_EVENTS.len() => "ok",
                _ => "partial",
            },
            format!("session state, prompts and tickets from every Claude session (~/.claude/settings.json, {wired}/{} events)", HOOK_EVENTS.len()),
        ),
        ("Claude MCP", "planned", "the cockpit tools as an MCP server; sessions use the cockpit CLI until then".into()),
        (
            "Cogitator",
            if cog.is_none() {
                "off"
            } else if cog_reads_cockpit {
                "ok"
            } else {
                "partial"
            },
            match (&cog, cog_reads_cockpit) {
                (None, _) => "daemon on :7777 not answering".into(),
                (_, true) => "bar, background and cat read the cockpit".into(),
                _ => "running, but not reading the cockpit".into(),
            },
        ),
        ("Ollama", if ollama.is_some() { "ok" } else { "off" }, "the cat's voice (localhost:11434)".into()),
        (
            "Palaver",
            if !ctx.palaver_bin.exists() {
                "off"
            } else if ep.palaver_sock.exists() {
                "ok"
            } else {
                "partial"
            },
            "meetings, transcripts and coach baseline via its MCP server".into(),
        ),
        (
            "Waypoint",
            if wp.is_some() { "partial" } else { "off" },
            if wp.is_some() { "running; the cockpit API is ready, Waypoint does not call it yet".into() } else { "not running (:5174); the cockpit API is ready for it".into() },
        ),
        ("Jira", "planned", "placeholder: sync tickets with a Jira project".into()),
        ("GitHub Issues", "planned", "placeholder: sync tickets with a GitHub repo".into()),
    ];
    Value::Array(
        rows.iter()
            .map(|(name, status, detail)| json!({"name": name, "status": status, "detail": detail, "help": help(name)}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::Tmux;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serves one canned 200 response per accepted connection, forever (until the process ends).
    fn serve(body: &'static str) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            }
        });
        addr
    }

    fn hooks_json(events: &[&str], cmd: &str) -> String {
        let hooks: serde_json::Map<String, Value> =
            events.iter().map(|e| (e.to_string(), json!([{"hooks": [{"type": "command", "command": cmd}]}]))).collect();
        json!({"hooks": hooks}).to_string()
    }

    #[test]
    fn detects_which_hook_events_call_cockpit() {
        assert_eq!(wired_hook_events(&hooks_json(&HOOK_EVENTS, "/Users/x/bin/cockpit hook")).len(), 7);
        assert_eq!(wired_hook_events(&hooks_json(&["Stop", "SessionEnd"], "cockpit hook")), ["Stop", "SessionEnd"]);
        assert!(
            wired_hook_events(&hooks_json(&HOOK_EVENTS, "node /x/tk.mjs hook")).is_empty(),
            "prototype entries do not count"
        );
        assert!(wired_hook_events("not json").is_empty());
    }

    #[test]
    fn status_reflects_neighbours_and_hooks() {
        let d = crate::db::tempdir::Dir::new();
        let mut ctx = Ctx::for_test(d.path(), Tmux::private("conn-unit"));
        let ep = Endpoints {
            cogitator: serve(r#"{"cockpit": true}"#),
            waypoint: "127.0.0.1:1".into(),
            ollama: serve("{}"),
            palaver_sock: d.path().join("app.sock"),
        };
        std::fs::write(&ctx.claude_settings, hooks_json(&["Stop"], "cockpit hook")).unwrap();
        std::fs::write(&ctx.palaver_bin, "").unwrap();
        let by = |v: &Value, n: &str| v.as_array().unwrap().iter().find(|c| c["name"] == n).unwrap().clone();
        let s = status_with(&ctx, &ep);
        assert_eq!(by(&s, "Claude Code hooks")["status"], "partial");
        assert_eq!(by(&s, "Cogitator")["status"], "ok");
        assert_eq!(by(&s, "Ollama")["status"], "ok");
        assert_eq!(by(&s, "Waypoint")["status"], "off");
        assert_eq!(by(&s, "Palaver")["status"], "partial", "binary present, app socket missing");
        assert_eq!(by(&s, "Claude MCP")["status"], "planned");
        assert_eq!(s.as_array().unwrap().len(), 8);
        assert!(s.as_array().unwrap().iter().all(|c| !c["help"].as_str().unwrap().is_empty()));

        std::fs::write(d.path().join("app.sock"), "").unwrap();
        ctx.palaver_bin = d.path().join("missing");
        let ep2 = Endpoints { cogitator: serve(r#"{"cockpit": null}"#), ..ep };
        let s = status_with(&ctx, &ep2);
        assert_eq!(by(&s, "Cogitator")["status"], "partial");
        assert_eq!(by(&s, "Palaver")["status"], "off");
    }
}
