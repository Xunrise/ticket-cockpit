//! Claude Code sessions as the hooks report them: a small state machine plus the context
//! text injected at SessionStart.
//! States: working (prompt submitted or tool running), needs (permission prompt or open question),
//! turn (Claude stopped, waiting for the user), ended.
use crate::tickets;
use crate::tmux::{PaneRef, Tmux};
use crate::util::{git, now_iso, truncate};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// What the hook reads from its process environment, injectable for tests.
#[derive(Debug, Default, Clone)]
pub struct HookEnv {
    pub tmux: Option<String>,
    pub tmux_pane: Option<String>,
    pub tk_issue: Option<String>,
    /// How Claude should call the cockpit CLI.
    pub exe: String,
}

impl HookEnv {
    pub fn from_process() -> Self {
        HookEnv {
            tmux: std::env::var("TMUX").ok().filter(|s| !s.is_empty()),
            tmux_pane: std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty()),
            tk_issue: std::env::var("TK_ISSUE").ok().filter(|s| !s.is_empty()),
            exe: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "cockpit".into()),
        }
    }

    /// True when the process runs in the cockpit's own tmux server (the socket name is the TMUX path's basename).
    pub fn on_cockpit(&self, tmux: &Tmux) -> bool {
        self.tmux
            .as_deref()
            .and_then(|t| t.split(',').next())
            .and_then(|p| Path::new(p).file_name())
            .is_some_and(|n| n.to_string_lossy() == tmux.cockpit_sock)
    }
}

pub fn set_state(conn: &Connection, tmux: &Tmux, id: &str, state: &str, detail: &str) -> Result<()> {
    let before: Option<(Option<String>, Option<String>)> = conn
        .query_row("SELECT state, tmux_pane FROM sessions WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let now = now_iso();
    conn.execute(
        "UPDATE sessions SET state=?, state_detail=?, state_at=?, last_seen=? WHERE id=?",
        params![state, detail, now, now, id],
    )?;
    // mirrored onto the cockpit pane so tmux key bindings can depend on it (Esc while working asks first)
    if let Some((old, Some(pane))) = before {
        if pane.starts_with("cockpit:") && old.as_deref() != Some(state) {
            if let Ok(p) = PaneRef::parse(&pane) {
                let _ = tmux.set_pane_option(&p, "@tk_state", state);
            }
        }
    }
    Ok(())
}

fn questions(input: &Value) -> String {
    input["questions"]
        .as_array()
        .map(|qs| qs.iter().filter_map(|q| q["question"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// Last assistant text, or the open AskUserQuestion's questions, from the tail of a transcript.
pub fn last_assistant_text(path: &str) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(300_000))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    for line in String::from_utf8_lossy(&buf).lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let m = &v["message"];
        let Some(content) = m["content"].as_array().filter(|_| m["role"] == "assistant") else { continue };
        let ask = content.iter().find(|c| c["type"] == "tool_use" && c["name"] == "AskUserQuestion");
        let text = match ask {
            Some(a) => questions(&a["input"]),
            None => content
                .iter()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string(),
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

pub fn describe_tool(name: &str, input: &Value) -> String {
    let arg = ["file_path", "command", "pattern", "description", "url"]
        .iter()
        .find_map(|k| {
            input[k]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| if *k == "file_path" { basename(s) } else { s.to_string() })
        })
        .unwrap_or_default();
    format!("{name} {}", truncate(arg.lines().next().unwrap_or(""), 60)).trim().to_string()
}

fn basename(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// Handles one hook event. Returns text for Claude's context (SessionStart only).
pub fn handle(conn: &Connection, tmux: &Tmux, env: &HookEnv, h: &Value) -> Result<String> {
    let sid = s(h, "session_id");
    if sid.is_empty() {
        return Ok(String::new());
    }
    let cwd = s(h, "cwd");
    let event = s(h, "hook_event_name");
    // sessions that started before the cockpit was installed (or while it was down) register on their next event
    if event != "SessionStart" && event != "SessionEnd" {
        let known: bool =
            conn.query_row("SELECT count(*) FROM sessions WHERE id=?", [sid], |r| r.get::<_, i64>(0))? > 0;
        if !known {
            register(conn, tmux, env, h)?;
        }
    }
    match event {
        "SessionStart" => return session_start(conn, tmux, env, h),
        "Stop" => {
            let msg = Some(s(h, "last_assistant_message").to_string())
                .filter(|m| !m.is_empty())
                .or_else(|| last_assistant_text(s(h, "transcript_path")));
            conn.execute(
                "UPDATE sessions SET head_end=?, last_message=? WHERE id=?",
                params![git(cwd, &["rev-parse", "HEAD"]), msg.map(|m| truncate(&m, 400)), sid],
            )?;
            set_state(conn, tmux, sid, "turn", "")?;
        }
        "SessionEnd" => {
            conn.execute(
                "UPDATE sessions SET ended=?, head_end=? WHERE id=?",
                params![now_iso(), git(cwd, &["rev-parse", "HEAD"]), sid],
            )?;
            set_state(conn, tmux, sid, "ended", "")?;
        }
        "UserPromptSubmit" => {
            conn.execute("INSERT INTO prompts VALUES(?,?)", params![sid, now_iso()])?;
            conn.execute("UPDATE sessions SET last_prompt=? WHERE id=?", params![truncate(s(h, "prompt"), 300), sid])?;
            set_state(conn, tmux, sid, "working", "thinking")?;
        }
        ev @ ("PreToolUse" | "PostToolUse") => {
            let what = describe_tool(s(h, "tool_name"), &h["tool_input"]);
            if ev == "PreToolUse" {
                let raw: Option<String> =
                    conn.query_row("SELECT trail FROM sessions WHERE id=?", [sid], |r| r.get(0)).optional()?.flatten();
                let mut trail: Vec<Value> = raw.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
                trail.push(json!({"what": what, "at": now_iso()}));
                let keep = trail.split_off(trail.len().saturating_sub(5));
                conn.execute("UPDATE sessions SET trail=? WHERE id=?", params![serde_json::to_string(&keep)?, sid])?;
            }
            if ev == "PreToolUse" && s(h, "tool_name") == "AskUserQuestion" {
                conn.execute(
                    "UPDATE sessions SET last_message=? WHERE id=?",
                    params![questions(&h["tool_input"]), sid],
                )?;
                set_state(conn, tmux, sid, "needs", "asks a question")?;
            } else {
                set_state(conn, tmux, sid, "working", &what)?;
            }
        }
        "Notification" => {
            let msg = s(h, "message");
            if s(h, "notification_type") == "permission_prompt" || msg.to_lowercase().contains("permission") {
                set_state(conn, tmux, sid, "needs", if msg.is_empty() { "permission" } else { msg })?;
            }
        }
        _ => {}
    }
    Ok(String::new())
}

/// Records a session (repo, cwd, start HEAD, tmux pane, transcript). Returns whether it runs in the cockpit's tmux.
fn register(conn: &Connection, tmux: &Tmux, env: &HookEnv, h: &Value) -> Result<bool> {
    let (sid, cwd) = (s(h, "session_id"), s(h, "cwd"));
    let now = now_iso();
    conn.execute(
        "INSERT INTO sessions(id,repo,cwd,started,last_seen,head_start) VALUES(?,?,?,?,?,?)
         ON CONFLICT(id) DO UPDATE SET last_seen=excluded.last_seen",
        params![sid, tickets::repo_at(conn, cwd)?, cwd, now, now, git(cwd, &["rev-parse", "HEAD"])],
    )?;
    let on_cockpit = env.on_cockpit(tmux);
    if let Some(pane) = &env.tmux_pane {
        let pane = format!("{}{pane}", if on_cockpit { "cockpit:" } else { "" });
        conn.execute("UPDATE sessions SET tmux_pane=? WHERE id=?", params![pane, sid])?;
    }
    conn.execute("UPDATE sessions SET transcript=? WHERE id=?", params![h["transcript_path"].as_str(), sid])?;
    Ok(on_cockpit)
}

fn session_start(conn: &Connection, tmux: &Tmux, env: &HookEnv, h: &Value) -> Result<String> {
    let sid = s(h, "session_id");
    let on_cockpit = register(conn, tmux, env, h)?;
    let issue = env.tk_issue.as_deref().map(|k| tickets::find(conn, k)).transpose()?.flatten();
    if let Some(t) = &issue {
        if t.status != "doing" || t.session.as_deref() != Some(sid) {
            tickets::claim(conn, &t.key, sid)?;
        }
    }
    set_state(conn, tmux, sid, "turn", "")?;
    Ok(context_text(env, sid, issue.as_ref(), on_cockpit))
}

/// What Claude is told at SessionStart: how to use the cockpit tools, plus the project and tmux paragraphs when they apply.
pub fn context_text(env: &HookEnv, sid: &str, issue: Option<&tickets::Ticket>, on_cockpit: bool) -> String {
    let x = &env.exe;
    let mut t = format!(
        "Local ticket tracker (cockpit). Session id: {sid}. Commands: `{x} ready`, `{x} show KEY`, `{x} add \"title\" \"body\"`,\n`{x} claim KEY --session {sid}`, `{x} comment KEY \"text\" --session {sid}`, `{x} close KEY --summary \"...\" --session {sid}`.\nWhen you work on a ticket, claim it first and close it with a short summary when done."
    );
    t += &format!("\nIf you hit friction or a bug in the cockpit itself, file it with `{x} tool cockpit.feedback '{{\"text\":\"…\"}}'` instead of working around it.");
    if let Some(i) = issue {
        t += &format!("\nThis session was started for ticket {} (already claimed): {}\n{}", i.key, i.title, i.body);
    }
    if let Some(p) = issue.and_then(|i| i.project.as_deref()) {
        t += &format!(
            "\n\nThis session belongs to the cockpit project \"{p}\". When the user mentions an ambition, an idea or a task that does not belong in this session's work, add it to the project for a later or parallel session instead of doing it now: `{x} tool ticket.add '{{\"project\":\"{p}\",\"title\":\"…\",\"body\":\"…\",\"session\":\"{sid}\"}}'`, and tell the user in one line which ticket you added. `{x} tool project.overview '{{\"project\":\"{p}\"}}'` shows what is planned and done."
        );
    }
    if on_cockpit {
        let project = issue.and_then(|i| i.project.as_deref()).map(|p| format!(" (project {p})")).unwrap_or_default();
        t += &format!(
            "\n\nYou run inside the cockpit's tmux (socket \"cockpit\"), in a window next to the user's queue. The user watches this window.\nNever start servers, watchers or other long-running commands as background Bash tasks. Put each in its own named pane of this window instead:\n- `{x} tool pane.run '{{\"name\":\"backend\",\"cmd\":\"npm run dev\"}}'` starts (or restarts) a pane; add \"cwd\" for a subfolder\n- `{x} tool pane.up` starts the project's configured default panes{project}\n- `{x} tool pane.list`, `{x} tool pane.logs '{{\"name\":\"backend\"}}'` to read its output, `{x} tool pane.stop '{{\"name\":\"backend\"}}'`\n- `{x} tool project.set_services '{{\"project\":\"…\",\"services\":[{{\"name\":\"…\",\"cmd\":\"…\"}}]}}'` to change the defaults when the user asks."
        );
    }
    t
}

/// The session's tmux pane, when it still exists.
pub fn live_pane(conn: &Connection, tmux: &Tmux, sid: &str) -> Option<PaneRef> {
    let pane: Option<String> =
        conn.query_row("SELECT tmux_pane FROM sessions WHERE id=?", [sid], |r| r.get(0)).optional().ok()??;
    let p = PaneRef::parse(&pane?).ok()?;
    tmux.pane_alive(&p).then_some(p)
}

/// Entry point for `cockpit hook`: never fails, never blocks a session.
pub fn run_hook(stdin: &str) {
    let result = std::panic::catch_unwind(|| -> Result<String> {
        let h: Value = serde_json::from_str(stdin)?;
        let conn = crate::db::open(&crate::paths::db_path())?;
        handle(&conn, &Tmux::system(), &HookEnv::from_process(), &h)
    });
    match result {
        Ok(Ok(out)) if !out.is_empty() => println!("{out}"),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => eprintln!("cockpit hook: {e:#}"),
        Err(_) => eprintln!("cockpit hook: panic"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::temp_db;
    use crate::tickets::{create, NewTicket};
    use crate::tmux::testutil::guard;

    fn env() -> HookEnv {
        HookEnv { exe: "cockpit".into(), ..Default::default() }
    }

    fn ev(name: &str, extra: Value) -> Value {
        let mut v = json!({"session_id": "s1", "cwd": "/tmp", "hook_event_name": name});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    }

    fn state(c: &Connection) -> (String, String) {
        c.query_row("SELECT state, state_detail FROM sessions WHERE id='s1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    }

    fn start(c: &Connection, t: &Tmux, e: &HookEnv) -> String {
        handle(c, t, e, &ev("SessionStart", json!({"transcript_path": "/x.jsonl"}))).unwrap()
    }

    #[test]
    fn a_session_unknown_to_the_cockpit_registers_on_its_next_event() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        handle(&c, &t, &env(), &ev("UserPromptSubmit", json!({"prompt": "hi", "transcript_path": "/y.jsonl"})))
            .unwrap();
        assert_eq!(state(&c), ("working".into(), "thinking".into()));
        let (cwd, tr): (String, String) = c
            .query_row("SELECT cwd, transcript FROM sessions WHERE id='s1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((cwd.as_str(), tr.as_str()), ("/tmp", "/y.jsonl"));
        // SessionEnd of an unknown session does not create one
        handle(&c, &t, &env(), &json!({"session_id": "s2", "cwd": "/tmp", "hook_event_name": "SessionEnd"})).unwrap();
        assert_eq!(c.query_row("SELECT count(*) FROM sessions WHERE id='s2'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn the_start_context_tells_sessions_how_to_report_friction() {
        let text = context_text(&env(), "s1", None, true);
        assert!(text.contains("tool cockpit.feedback") && text.contains("instead of working around it"), "{text}");
    }

    #[test]
    fn events_drive_the_state() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        start(&c, &t, &env());
        assert_eq!(state(&c), ("turn".into(), "".into()));
        handle(&c, &t, &env(), &ev("UserPromptSubmit", json!({"prompt": "do it"}))).unwrap();
        assert_eq!(state(&c), ("working".into(), "thinking".into()));
        handle(
            &c,
            &t,
            &env(),
            &ev("PreToolUse", json!({"tool_name": "Bash", "tool_input": {"command": "ls -la\nsecond"}})),
        )
        .unwrap();
        assert_eq!(state(&c), ("working".into(), "Bash ls -la".into()));
        handle(&c, &t, &env(), &ev("Notification", json!({"notification_type": "idle_prompt", "message": "waiting"})))
            .unwrap();
        assert_eq!(state(&c).0, "working", "only permission prompts need the user");
        handle(&c, &t, &env(), &ev("Notification", json!({"message": "Claude needs your permission to use Bash"})))
            .unwrap();
        assert_eq!(state(&c).0, "needs");
        handle(&c, &t, &env(), &ev("PostToolUse", json!({"tool_name": "Bash", "tool_input": {}}))).unwrap();
        assert_eq!(state(&c).0, "working");
        handle(&c, &t, &env(), &ev("Stop", json!({"last_assistant_message": "finished"}))).unwrap();
        assert_eq!(state(&c).0, "turn");
        let (msg, prompt): (String, String) = c
            .query_row("SELECT last_message, last_prompt FROM sessions WHERE id='s1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((msg.as_str(), prompt.as_str()), ("finished", "do it"));
        handle(&c, &t, &env(), &ev("SessionEnd", json!({}))).unwrap();
        assert_eq!(state(&c).0, "ended");
        let ended: Option<String> = c.query_row("SELECT ended FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        assert!(ended.is_some());
        let prompts: i64 = c.query_row("SELECT count(*) FROM prompts", [], |r| r.get(0)).unwrap();
        assert_eq!(prompts, 1);
    }

    #[test]
    fn ask_user_question_needs_you_with_the_questions_as_message() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        start(&c, &t, &env());
        let input = json!({"questions": [{"question": "Which db?"}, {"question": "Which port?"}]});
        handle(&c, &t, &env(), &ev("PreToolUse", json!({"tool_name": "AskUserQuestion", "tool_input": input})))
            .unwrap();
        assert_eq!(state(&c), ("needs".into(), "asks a question".into()));
        let msg: String = c.query_row("SELECT last_message FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!(msg, "Which db?\nWhich port?");
        handle(&c, &t, &env(), &ev("PostToolUse", json!({"tool_name": "AskUserQuestion", "tool_input": input})))
            .unwrap();
        assert_eq!(state(&c).0, "working");
    }

    #[test]
    fn trail_keeps_the_last_five_calls() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        start(&c, &t, &env());
        for i in 0..8 {
            handle(
                &c,
                &t,
                &env(),
                &ev("PreToolUse", json!({"tool_name": "Read", "tool_input": {"file_path": format!("/a/f{i}.rs")}})),
            )
            .unwrap();
        }
        let trail: String = c.query_row("SELECT trail FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        let v: Vec<Value> = serde_json::from_str(&trail).unwrap();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0]["what"], "Read f3.rs");
        assert_eq!(v[4]["what"], "Read f7.rs");
    }

    #[test]
    fn stop_falls_back_to_the_transcript() {
        let (d, c) = temp_db();
        let t = Tmux::private("unit");
        start(&c, &t, &env());
        let path = d.path().join("t.jsonl");
        let lines = [
            json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "older answer"}]}}),
            json!({"message": {"role": "user", "content": [{"type": "text", "text": "q"}]}}),
            json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "final answer"}, {"type": "tool_use", "name": "Bash", "input": {}}]}}),
        ];
        std::fs::write(&path, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        handle(&c, &t, &env(), &ev("Stop", json!({"transcript_path": path}))).unwrap();
        let msg: String = c.query_row("SELECT last_message FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!(msg, "final answer");

        // an open AskUserQuestion as the last entry counts as the message
        let ask = json!({"message": {"role": "assistant", "content": [{"type": "tool_use", "name": "AskUserQuestion", "input": {"questions": [{"question": "Sure?"}]}}]}});
        std::fs::write(&path, format!("{}\n{}", lines[0], ask)).unwrap();
        assert_eq!(last_assistant_text(path.to_str().unwrap()).as_deref(), Some("Sure?"));
        assert_eq!(last_assistant_text("/no/such/file"), None);
    }

    #[test]
    fn session_start_claims_ticket_and_context_depends_on_project_and_tmux() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        let plain = create(
            &c,
            NewTicket { repo: "r".into(), title: "plain".into(), body: "details".into(), ..Default::default() },
        )
        .unwrap();
        let proj = create(
            &c,
            NewTicket {
                repo: "r".into(),
                title: "in project".into(),
                project: Some("Alpha".into()),
                ..Default::default()
            },
        )
        .unwrap();

        // no ticket, outside the cockpit: just the tracker paragraph
        let out = start(&c, &t, &env());
        assert!(out.contains("Session id: s1") && out.contains("cockpit ready"));
        assert!(!out.contains("project") && !out.contains("named pane"));

        // ticket without a project
        let e = HookEnv { tk_issue: Some(plain.clone()), ..env() };
        let out = start(&c, &t, &e);
        assert!(out.contains(&format!("started for ticket {plain}")) && out.contains("details"));
        assert!(!out.contains("cockpit project"));
        let tk = tickets::get(&c, &plain).unwrap();
        assert_eq!((tk.status.as_str(), tk.session.as_deref()), ("doing", Some("s1")));

        // ticket with a project, on the cockpit's tmux
        let sock = format!("/tmp/tmux-501/{},123,0", t.cockpit_sock);
        let e = HookEnv { tk_issue: Some(proj.clone()), tmux: Some(sock), tmux_pane: Some("%7".into()), ..env() };
        let out = start(&c, &t, &e);
        assert!(out.contains("cockpit project \"Alpha\"") && out.contains("project.overview"));
        assert!(out.contains("Never start servers") && out.contains("pane.run") && out.contains("(project Alpha)"));
        let pane: String = c.query_row("SELECT tmux_pane FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!(pane, "cockpit:%7");

        // the user's default tmux gets a bare pane id and no cockpit paragraph
        let e = HookEnv { tmux: Some("/tmp/tmux-501/default,1,0".into()), tmux_pane: Some("%2".into()), ..env() };
        let out = start(&c, &t, &e);
        assert!(!out.contains("Never start servers"));
        let pane: String = c.query_row("SELECT tmux_pane FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!(pane, "%2");
    }

    #[test]
    fn repeated_session_start_keeps_head_and_does_not_reclaim() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        let k = create(&c, NewTicket { repo: "r".into(), title: "x".into(), ..Default::default() }).unwrap();
        let e = HookEnv { tk_issue: Some(k.clone()), ..env() };
        start(&c, &t, &e);
        start(&c, &t, &e);
        let claims = tickets::events(&c, &k).unwrap().iter().filter(|e| e["kind"] == "claimed").count();
        assert_eq!(claims, 1);
    }

    #[test]
    fn hook_mirrors_state_onto_the_cockpit_pane() {
        let Some(g) = guard() else { return };
        let t = &g.0;
        let (_d, c) = temp_db();
        crate::panes::cockpit_window(t, "W", "/tmp", &[], "cat").unwrap();
        let id = t.run(crate::tmux::Server::Cockpit, &["list-panes", "-t", "=cockpit:W", "-F", "#{pane_id}"]).unwrap();
        let e = HookEnv { tmux: Some(format!("/x/{},1,0", t.cockpit_sock)), tmux_pane: Some(id.clone()), ..env() };
        start(&c, t, &e);
        let opt = || t.run(crate::tmux::Server::Cockpit, &["display", "-p", "-t", &id, "#{@tk_state}"]).unwrap();
        assert_eq!(opt(), "turn");
        handle(&c, t, &e, &ev("UserPromptSubmit", json!({"prompt": "x"}))).unwrap();
        assert_eq!(opt(), "working");
        handle(&c, t, &e, &ev("Notification", json!({"notification_type": "permission_prompt", "message": "perm"})))
            .unwrap();
        assert_eq!(opt(), "needs");
    }

    #[test]
    fn hook_input_without_session_is_ignored() {
        let (_d, c) = temp_db();
        let t = Tmux::private("unit");
        assert_eq!(handle(&c, &t, &env(), &json!({"hook_event_name": "Stop"})).unwrap(), "");
        let n: i64 = c.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
