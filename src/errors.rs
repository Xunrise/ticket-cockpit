//! The error log: failed tool calls and daemon HTTP errors, kept so the cockpit can be improved from them.
use crate::ctx::Ctx;
use crate::util::{now_iso, truncate};
use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::Value;

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
