//! Palaver through its own MCP server (`palaver mcp`, stdio JSON-RPC), one process per call.
//! ponytail: spawn per call (~1 s); keep one client alive if pickers feel slow.
use crate::tickets::{self, Ticket};
use crate::tmux::Tmux;
use crate::{panes, projects, sessions, settings};
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Calls one MCP tool and returns the JSON payload of each text content block.
pub fn call(bin: &Path, tool: &str, args: &Value, timeout: Duration) -> Result<Vec<Value>> {
    let mut child = Command::new(bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("cannot start Palaver ({}): {e}", bin.display()))?;
    let result = exchange(&mut child, tool, args, timeout);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn exchange(child: &mut std::process::Child, tool: &str, args: &Value, timeout: Duration) -> Result<Vec<Value>> {
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let deadline = Instant::now() + timeout;
    let mut send = |m: Value| -> Result<()> {
        let mut m = m;
        m["jsonrpc"] = json!("2.0");
        writeln!(stdin, "{m}")?;
        Ok(stdin.flush()?)
    };
    let wait_for = |id: i64| -> Result<Value> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = rx.recv_timeout(left).map_err(|_| anyhow!("Palaver did not answer in time"))?;
            if m["id"] == id {
                return Ok(m);
            }
        }
    };
    send(
        json!({"id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cockpit", "version": "0"}}}),
    )?;
    wait_for(1)?;
    send(json!({"method": "notifications/initialized"}))?;
    send(json!({"id": 2, "method": "tools/call", "params": {"name": tool, "arguments": args}}))?;
    let m = wait_for(2)?;
    let content = m["result"]["content"].as_array().cloned().unwrap_or_default();
    if m["error"].is_object() || m["result"]["isError"] == true {
        let msg = m["error"]["message"]
            .as_str()
            .or_else(|| content.first().and_then(|c| c["text"].as_str()))
            .unwrap_or("Palaver error");
        bail!("{msg}");
    }
    Ok(content
        .iter()
        .filter_map(|c| c["text"].as_str())
        .map(|t| serde_json::from_str(t).unwrap_or_else(|_| Value::String(t.to_string())))
        .collect())
}

fn tool(bin: &Path, name: &str, args: Value) -> Result<Vec<Value>> {
    call(bin, name, &args, Duration::from_secs(20))
}

pub fn projects(bin: &Path) -> Result<Vec<Value>> {
    Ok(tool(bin, "list_projects", json!({}))?
        .iter()
        .map(|p| json!({"name": p["name"], "meetings": p["meetings"], "latest": p["latest"], "description": p["description"]}))
        .collect())
}

fn palaver_project_of(conn: &Connection, key: &str) -> Result<String> {
    let t = tickets::get(conn, key)?;
    let project = t.project.ok_or_else(|| anyhow!("{key} has no project"))?;
    projects::palaver_of(conn, &project)?.ok_or_else(|| anyhow!("project {project} is not linked to Palaver"))
}

/// Meetings of the ticket's Palaver project, newest first, with the ones already linked marked.
pub fn meetings(conn: &Connection, bin: &Path, key: &str, limit: i64) -> Result<Vec<Value>> {
    let linked: Vec<String> = tickets::links(conn, key)?
        .iter()
        .filter(|l| l["kind"] == "meeting")
        .filter_map(|l| l["ref"].as_str().map(String::from))
        .collect();
    let pv = palaver_project_of(conn, key)?;
    Ok(tool(bin, "list_meetings", json!({"project": pv, "limit": limit}))?
        .iter()
        .map(|m| {
            json!({"id": m["id"], "title": m["title"], "start": m["start"], "minutes": m["minutes"], "summary": m["summary"],
                   "linked": m["id"].as_str().is_some_and(|i| linked.iter().any(|l| l == i))})
        })
        .collect())
}

fn bullet_list(x: &Value) -> String {
    let items: Vec<&Value> = match x {
        Value::Array(a) => a.iter().collect(),
        Value::Null => vec![],
        other => vec![other],
    };
    items
        .iter()
        .map(|i| match i {
            Value::String(s) => format!("- {s}"),
            _ if i["task"].is_string() => {
                let owner = i["owner"].as_str().map(|o| format!(" ({o})")).unwrap_or_default();
                format!("- {}{owner}", i["task"].as_str().unwrap_or(""))
            }
            _ if i["text"].is_string() => format!("- {}", i["text"].as_str().unwrap_or("")),
            _ => format!("- {i}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_of(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| if v.is_null() { String::new() } else { v.to_string() })
}

/// A meeting as Markdown: notes first, transcript last.
pub fn meeting_markdown(m: &Value) -> String {
    let n = &m["notes"];
    let transcript = match &m["transcript"] {
        Value::Array(lines) => lines
            .iter()
            .map(|l| match l {
                Value::String(s) => s.clone(),
                _ => format!(
                    "{}: {}",
                    l["speaker"].as_str().unwrap_or(""),
                    l["text"].as_str().map(String::from).unwrap_or_else(|| l.to_string())
                ),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => text_of(other),
    };
    format!(
        "# {}\n\n{} · {} min · Palaver meeting {}\n\n## Summary\n\n{}\n\n## Next steps\n\n{}\n\n## Decisions\n\n{}\n\n## Transcript\n\n{}\n",
        text_of(&m["title"]),
        text_of(&m["start"]),
        text_of(&m["minutes"]),
        text_of(&m["id"]),
        text_of(&n["summary"]),
        bullet_list(&n["next_steps"]),
        bullet_list(&n["decisions"]),
        transcript
    )
}

/// Saves a meeting's notes and transcript, links it to the ticket and (setting on) pastes a reference
/// into the ticket's live Claude prompt without submitting it.
pub fn link_meeting(
    conn: &Connection,
    tmux: &Tmux,
    bin: &Path,
    context_dir: &Path,
    key: &str,
    meeting_id: &str,
    why: &str,
) -> Result<Value> {
    tickets::get(conn, key)?;
    let m = tool(bin, "get_meeting", json!({"meeting_id": meeting_id, "include_transcript": true}))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no meeting {meeting_id}"))?;
    std::fs::create_dir_all(context_dir)?;
    let path = context_dir.join(format!("{meeting_id}.md"));
    std::fs::write(&path, meeting_markdown(&m))?;
    let title = text_of(&m["title"]);
    tickets::add_link(conn, key, "meeting", meeting_id, &title, &path.to_string_lossy(), why)?;
    tickets::event(conn, key, None, "context", &format!("meeting: {title}"))?;
    let pane = if settings::get_bool(conn, "paste_meeting_context") {
        live_session_pane(conn, tmux, &tickets::get(conn, key)?)
    } else {
        None
    };
    if let Some(p) = &pane {
        let day = text_of(&m["start"]).chars().take(10).collect::<String>();
        let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
        tmux.paste(
            p,
            &format!(
                "Context for {key} from the meeting \"{title}\" ({day}): notes and transcript are in {}{why}. ",
                path.display()
            ),
        )?;
    }
    Ok(json!({"key": key, "meeting_id": meeting_id, "title": title, "path": path, "pasted": pane.is_some()}))
}

fn live_session_pane(conn: &Connection, tmux: &Tmux, t: &Ticket) -> Option<crate::tmux::PaneRef> {
    if t.status != "doing" {
        return None;
    }
    sessions::live_pane(conn, tmux, t.session.as_deref()?)
}

/// Starts a Claude session (`KEY-ctx` window) that searches the project's meetings and links the relevant ones.
pub fn auto_link(conn: &Connection, tmux: &Tmux, exe: &str, key: &str) -> Result<Value> {
    let t = tickets::get(conn, key)?;
    let pv = palaver_project_of(conn, key)?;
    let prompt = format!(
        "Find meetings relevant to ticket {key} (\"{}\": {}). Use the palaver MCP tools: list_meetings and search_meetings with project \"{pv}\", and get_meeting to read candidates. For each meeting that matters for this ticket run: {exe} tool ticket.link_meeting '{{\"key\":\"{key}\",\"meeting_id\":\"<id>\",\"why\":\"<one line>\"}}'. Link only meetings with concrete input for this ticket. End with the list you linked and why.",
        t.title, t.body
    );
    Ok(json!({"window": panes::run_claude(conn, tmux, &t.repo, &format!("{key}-ctx"), &prompt)?}))
}

/// The speaking-coach baseline Palaver writes to disk.
pub fn coach_baseline(path: &Path) -> Option<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(v["baseline"].clone()).filter(|b| !b.is_null())
}

/// The latest call and the coach baseline; either side is null when unavailable.
/// ponytail: per-call coach feedback lives behind Palaver's app API; this reads the baseline file until Palaver exposes coaching over MCP.
pub fn calls(bin: &Path, coach_file: &Path) -> Value {
    let last = tool(bin, "list_meetings", json!({"limit": 1})).ok().and_then(|v| v.into_iter().next()).map(|m| {
        json!({"id": m["id"], "title": m["title"], "start": m["start"], "minutes": m["minutes"], "projects": m["projects"]})
    });
    json!({"last": last, "baseline": coach_baseline(coach_file)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use std::os::unix::fs::PermissionsExt;

    const STUB: &str = r#"#!/bin/sh
read l1
echo '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stub","version":"0"}}}'
read l2
read l3
case "$l3" in
  *'"limit":1'*'"name":"list_meetings"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"id\":\"2026-11-18T10-00-00\",\"title\":\"Standup\",\"start\":\"2026-11-18T10:00:00\",\"minutes\":15,\"projects\":[\"Alpha\"]}"}]}}';;
  *'"name":"list_projects"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"name\":\"Alpha\",\"meetings\":3}"},{"type":"text","text":"{\"name\":\"Beta\",\"meetings\":1}"}]}}';;
  *'"name":"get_meeting"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"id\":\"m1\",\"title\":\"Design review\",\"start\":\"2026-11-18T10:00:00\",\"minutes\":30,\"notes\":{\"summary\":\"We agreed.\",\"next_steps\":[{\"task\":\"Ship it\",\"owner\":\"Ann\"},\"Write docs\"],\"decisions\":[\"Use SQLite\"]},\"transcript\":[{\"speaker\":\"Ann\",\"text\":\"Hello\"}]}"}]}}';;
  *'"name":"fail"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"isError":true,"content":[{"type":"text","text":"project not found"}]}}';;
  *'"name":"rpc_error"'*) echo '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no such tool"}}';;
  *'"name":"silent"'*) sleep 5;;
esac
"#;

    pub fn stub(dir: &Dir) -> std::path::PathBuf {
        let p = dir.path().join("palaver-stub");
        std::fs::write(&p, STUB).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn t(bin: &Path, name: &str, args: Value) -> Result<Vec<Value>> {
        call(bin, name, &args, Duration::from_secs(5))
    }

    #[test]
    fn calls_a_tool_and_parses_each_content_block() {
        let d = Dir::new();
        let bin = stub(&d);
        let m = t(&bin, "list_meetings", json!({"limit": 1})).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["title"], "Standup");
        let p = t(&bin, "list_projects", json!({})).unwrap();
        assert_eq!(p.iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>(), ["Alpha", "Beta"]);
    }

    #[test]
    fn tool_errors_and_timeouts_surface() {
        let d = Dir::new();
        let bin = stub(&d);
        assert_eq!(t(&bin, "fail", json!({})).unwrap_err().to_string(), "project not found");
        assert_eq!(t(&bin, "rpc_error", json!({})).unwrap_err().to_string(), "no such tool");
        let e = call(&bin, "silent", &json!({}), Duration::from_millis(300)).unwrap_err();
        assert!(e.to_string().contains("did not answer"));
        assert!(t(Path::new("/no/such/palaver"), "x", json!({}))
            .unwrap_err()
            .to_string()
            .contains("cannot start Palaver"));
    }

    #[test]
    fn meeting_becomes_markdown() {
        let d = Dir::new();
        let bin = stub(&d);
        let m = t(&bin, "get_meeting", json!({})).unwrap().remove(0);
        let md = meeting_markdown(&m);
        assert!(md.starts_with("# Design review\n\n2026-11-18T10:00:00 · 30 min · Palaver meeting m1"));
        assert!(md.contains("## Summary\n\nWe agreed."));
        assert!(md.contains("- Ship it (Ann)\n- Write docs"));
        assert!(md.contains("## Decisions\n\n- Use SQLite"));
        assert!(md.ends_with("## Transcript\n\nAnn: Hello\n"));
    }

    #[test]
    fn coach_baseline_comes_from_the_file() {
        let d = Dir::new();
        let f = d.path().join("coach-baseline.json");
        assert_eq!(coach_baseline(&f), None);
        std::fs::write(&f, r#"{"baseline":{"wpm":140,"filler_per_min":2.5}}"#).unwrap();
        assert_eq!(coach_baseline(&f).unwrap()["wpm"], 140);
        std::fs::write(&f, "{}").unwrap();
        assert_eq!(coach_baseline(&f), None);
    }

    #[test]
    fn calls_combine_latest_meeting_and_baseline() {
        let d = Dir::new();
        let bin = stub(&d);
        let f = d.path().join("b.json");
        std::fs::write(&f, r#"{"baseline":{"wpm":120}}"#).unwrap();
        let c = calls(&bin, &f);
        assert_eq!(c["last"]["title"], "Standup");
        assert_eq!(c["baseline"]["wpm"], 120);
        let none = calls(Path::new("/no/palaver"), &d.path().join("none.json"));
        assert!(none["last"].is_null() && none["baseline"].is_null());
    }

    #[test]
    fn link_meeting_saves_the_file_and_the_link() {
        let d = Dir::new();
        let bin = stub(&d);
        let (_dd, conn) = crate::db::temp_db();
        let key =
            tickets::create(&conn, tickets::NewTicket { repo: "r".into(), title: "x".into(), ..Default::default() })
                .unwrap();
        let ctx = d.path().join("ctx");
        let out = link_meeting(&conn, &Tmux::private("unit"), &bin, &ctx, &key, "m1", "design input").unwrap();
        assert_eq!(out["pasted"], false);
        assert!(std::fs::read_to_string(ctx.join("m1.md")).unwrap().contains("Design review"));
        let links = tickets::links(&conn, &key).unwrap();
        assert_eq!(
            (links[0]["kind"].as_str(), links[0]["title"].as_str(), links[0]["why"].as_str()),
            (Some("meeting"), Some("Design review"), Some("design input"))
        );
        assert!(tickets::events(&conn, &key).unwrap().iter().any(|e| e["kind"] == "context"));
    }
}
