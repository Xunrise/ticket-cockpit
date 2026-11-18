//! The error log: failed tool calls and daemon HTTP errors, kept so the cockpit can be improved from them.
use crate::ctx::Ctx;
use crate::tickets::{self, NewTicket};
use crate::util::{now_iso, truncate};
use crate::{projects, settings};
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub fn log(conn: &Connection, source: &str, text: &str, context: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO errors(at,source,text,context) VALUES(?,?,?,?)",
        params![now_iso(), source, text, context],
    )?;
    Ok(())
}

/// Best effort: failing to log must never hide the original error.
pub fn log_quiet(ctx: &Ctx, source: &str, text: &str, context: &str) {
    if let Ok(conn) = ctx.db() {
        let _ = log(&conn, source, text, context);
    }
}

/// Expected answers are not errors: a refused send (draft in the prompt, open question).
pub fn expected(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::prompt::SendRefused>().is_some()
}

/// The last `n` errors, newest first.
pub fn recent(conn: &Connection, n: usize) -> Result<Vec<Value>> {
    crate::db::rows(conn, "SELECT at, source, text, context FROM errors ORDER BY id DESC LIMIT ?", [n as i64])
}

fn render_context(ctx: &Value) -> String {
    match ctx {
        Value::Object(o) => o
            .iter()
            .map(|(k, v)| format!("- {k}: {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Files a ticket in the cockpit project, with a Context section. Empty text files the last error as "Fix: <error>".
pub fn feedback(c: &Ctx, text: &str, context: &Value) -> Result<Value> {
    let conn = c.db()?;
    let last = recent(&conn, 1)?.into_iter().next();
    let last_line =
        last.as_ref().map(|e| format!("{} ({})", e["text"].as_str().unwrap_or(""), e["at"].as_str().unwrap_or("")));
    let (title, mut body) = if text.trim().is_empty() {
        let e = last.as_ref().ok_or_else(|| anyhow!("nothing to file: type a text, or hit an error first"))?;
        let t = e["text"].as_str().unwrap_or("");
        (
            format!("Fix: {}", truncate(t.lines().next().unwrap_or(""), 90)),
            format!("{t}\n\nerror context: {}", e["context"].as_str().unwrap_or("")),
        )
    } else {
        (truncate(text.lines().next().unwrap_or(""), 90), text.trim().to_string())
    };
    body += "\n\n## Context\n";
    let rendered = render_context(context);
    if !rendered.is_empty() {
        body += &format!("{rendered}\n");
    }
    body += &format!(
        "- last error: {}\n- build: {} ({})\n",
        last_line.as_deref().unwrap_or("none"),
        crate::util::build_id(),
        crate::update::installed()
    );
    let project = settings::get(&conn, "cockpit_project")?.as_str().unwrap_or("ticket-cockpit").to_string();
    let repo_path = settings::get(&conn, "cockpit_repo")?.as_str().unwrap_or_default().to_string();
    tickets::ensure_project(&conn, &project)?;
    let repo = match projects::main_repo(&conn, &project)? {
        Some(r) => r,
        None if !repo_path.is_empty() => tickets::remember_repo(&conn, &repo_path)?,
        None => bail!("no repo for project {project}: set cockpit_repo"),
    };
    let key = tickets::create(
        &conn,
        NewTicket { repo, title: title.clone(), body, project: Some(project.clone()), ..Default::default() },
    )?;
    Ok(json!({"key": key, "project": project, "title": title}))
}

/// The recent errors as text for the housekeeping prompt.
pub fn digest(conn: &Connection) -> String {
    let rows = recent(conn, 50).unwrap_or_default();
    if rows.is_empty() {
        return "none".into();
    }
    rows.iter()
        .map(|e| {
            format!(
                "- {} [{}] {} ({})",
                e["at"].as_str().unwrap_or(""),
                e["source"].as_str().unwrap_or(""),
                truncate(e["text"].as_str().unwrap_or(""), 200),
                truncate(e["context"].as_str().unwrap_or(""), 120)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::tmux::Tmux;
    use serde_json::json;

    fn ctx() -> (Dir, Ctx) {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("errors-unit"));
        (d, c)
    }

    #[test]
    fn a_failing_tool_call_is_recorded() {
        let (_d, c) = ctx();
        assert!(crate::tools::call(&c, "ticket.get", json!({"key": "NOPE-1"})).is_err());
        assert!(crate::tools::call(&c, "no.such.tool", json!({})).is_err());
        let e = recent(&c.db().unwrap(), 10).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["source"], "tool");
        assert!(e[0]["text"].as_str().unwrap().contains("no tool no.such.tool"));
        assert!(e[1]["text"].as_str().unwrap().starts_with("ticket.get:"));
        assert!(e[1]["context"].as_str().unwrap().contains("NOPE-1"));
    }

    #[test]
    fn log_error_tool_records_and_never_logs_itself() {
        let (_d, c) = ctx();
        crate::tools::call(&c, "cockpit.log_error", json!({"text": "boom", "context": "x"})).unwrap();
        assert!(crate::tools::call(&c, "cockpit.log_error", json!({})).is_err());
        let e = crate::tools::call(&c, "cockpit.errors", json!({})).unwrap();
        assert_eq!(e.as_array().unwrap().len(), 1);
        assert_eq!(e[0]["text"], "boom");
    }

    #[test]
    fn a_send_refusal_is_not_an_error() {
        let refused: anyhow::Error = crate::prompt::SendRefused::Draft("x".into()).into();
        assert!(expected(&refused));
        assert!(expected(&refused.context("while sending")), "wrapping keeps the type");
        assert!(!expected(&anyhow::anyhow!("boom")));
    }

    fn git_repo(d: &Dir, name: &str) -> String {
        let p = d.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        crate::util::sh("git", &["init", "-q"], Some(&p));
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn feedback_lands_in_the_cockpit_project_with_context() {
        let (d, c) = ctx();
        let repo = git_repo(&d, "ticket-cockpit");
        crate::settings::set(&c.db().unwrap(), "cockpit_repo", &json!(repo)).unwrap();
        let r = crate::tools::call(
            &c,
            "cockpit.feedback",
            json!({"text": "q jumps\nwhen resized", "context": {"tab": "queue", "row": "TIC-1"}}),
        )
        .unwrap();
        assert_eq!((r["project"].as_str(), r["title"].as_str()), (Some("ticket-cockpit"), Some("q jumps")));
        let conn = c.db().unwrap();
        let t = tickets::get(&conn, r["key"].as_str().unwrap()).unwrap();
        assert_eq!(t.project.as_deref(), Some("ticket-cockpit"));
        assert!(t.body.starts_with("q jumps\nwhen resized\n\n## Context\n- tab: queue\n- row: TIC-1\n"), "{}", t.body);
        assert!(t.body.contains("- last error: none") && t.body.contains("- build:"));
    }

    #[test]
    fn empty_feedback_files_the_last_error_and_needs_one() {
        let (d, c) = ctx();
        let repo = git_repo(&d, "ticket-cockpit");
        crate::settings::set(&c.db().unwrap(), "cockpit_repo", &json!(repo)).unwrap();
        assert!(feedback(&c, "  ", &Value::Null).is_err(), "nothing to file");
        log(&c.db().unwrap(), "http", "send to s1: no pane", "ctx").unwrap();
        let r = feedback(&c, "", &json!({"tab": "queue"})).unwrap();
        assert_eq!(r["title"], "Fix: send to s1: no pane");
        let t = tickets::get(&c.db().unwrap(), r["key"].as_str().unwrap()).unwrap();
        assert!(t.body.contains("error context: ctx") && t.body.contains("- last error: send to s1: no pane"));
    }

    #[test]
    fn feedback_prefers_the_projects_home_repo_and_needs_some_repo() {
        let (d, c) = ctx();
        let conn = c.db().unwrap();
        crate::settings::set(&conn, "cockpit_repo", &json!("")).unwrap();
        assert!(feedback(&c, "x", &Value::Null).is_err(), "no repo anywhere");
        let home = git_repo(&d, "home");
        conn.execute("INSERT INTO repos(name,path) VALUES('home', ?)", [&home]).unwrap();
        projects::set_repo(&conn, "ticket-cockpit", "home").unwrap();
        let r = feedback(&c, "x", &Value::Null).unwrap();
        assert_eq!(tickets::get(&conn, r["key"].as_str().unwrap()).unwrap().repo, "home");
    }

    #[test]
    fn errors_returns_the_last_fifty() {
        let (_d, c) = ctx();
        let conn = c.db().unwrap();
        for i in 0..60 {
            log(&conn, "t", &format!("e{i}"), "").unwrap();
        }
        let e = crate::tools::call(&c, "cockpit.errors", json!({})).unwrap();
        assert_eq!(e.as_array().unwrap().len(), 50);
        assert_eq!(e[0]["text"], "e59");
        assert!(digest(&conn).contains("e59"));
    }
}
