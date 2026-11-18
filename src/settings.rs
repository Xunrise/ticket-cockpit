//! Typed settings: one JSON value per key, defaults and help texts in the table below.
//! Frontends read and write them through the `settings.*` tools.
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

pub struct Def {
    pub key: &'static str,
    pub default: fn() -> Value,
    pub desc: &'static str,
    pub help: &'static str,
}

pub const DEFS: &[Def] = &[
    Def {
        key: "queue_key",
        default: || json!("S-Escape"),
        desc: "tmux key that jumps back to the queue (e.g. S-Escape, C-], F12)",
        help: "Pressed anywhere in the cockpit (a Claude pane, a server pane), this key puts the cursor back on the queue.\n\nNames follow tmux: S-Escape is Shift+Esc, C-] is Ctrl+], M-q is Alt+q, F12 is F12.\n\nAlt combinations belong to AeroSpace on this machine, so they will not reach the cockpit.\nIf Shift+Esc does nothing, the terminal does not send it as its own key: pick F12 or C-] instead.",
    },
    Def {
        key: "confirm_interrupt",
        default: || json!(true),
        desc: "Ask before Esc interrupts a working Claude session",
        help: "Esc in a Claude session that is working would cancel what it is doing. With this on, a small menu asks first:\n\n  i  interrupt the running task\n  k  keep it running\n  n  interrupt, and never ask again (turns this setting off)\n\nWhen Claude is waiting for you (a question, your turn), Esc always goes straight through.",
    },
    Def {
        key: "queue_width",
        default: || json!(38),
        desc: "Width of the queue pane in percent",
        help: "How much of a ticket's window the queue takes when it sits next to the session. The rest is Claude and its server panes.",
    },
    Def {
        key: "start_services_on_dispatch",
        default: || json!(false),
        desc: "Start the project's default panes when a ticket's session starts",
        help: "Projects can have default panes (project.set_services), for example:\n\n  backend   npm run api\n  frontend  npm run dev   (in web/)\n\nWith this on, starting a ticket (s) also opens those panes in its window. Off: Claude or you start them when needed (pane.up).",
    },
    Def {
        key: "paste_meeting_context",
        default: || json!(true),
        desc: "Paste a linked meeting's reference into the session's prompt",
        help: "When you link a Palaver meeting to a ticket (l), the notes and transcript are saved as a file and linked either way.\n\nWith this on, a line pointing to that file is also typed into the ticket's live Claude prompt (not sent), so you can add a remark and press Enter.",
    },
    Def {
        key: "quick_replies",
        default: || json!("c=continue; y=yes; t=try again; m=merge it; d=start the dev server"),
        desc: "Queue keys that send a reply: key=text; …",
        help: "One key in the queue sends a whole reply to the selected session, as if you had typed it and pressed Enter.\n\nFormat: key=text, separated by semicolons. The keys are listed under each session's panel.\n\nThe cockpit will not send when the prompt already has your own text in it, or while Claude shows a question (use 1-4 there).",
    },
];

fn def(key: &str) -> Result<&'static Def> {
    DEFS.iter()
        .find(|d| d.key == key)
        .ok_or_else(|| anyhow!("no setting {key}"))
}

pub fn get(conn: &Connection, key: &str) -> Result<Value> {
    let d = def(key)?;
    let raw: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(d.default))
}

pub fn get_bool(conn: &Connection, key: &str) -> bool {
    get(conn, key)
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn get_i64(conn: &Connection, key: &str) -> i64 {
    get(conn, key).ok().and_then(|v| v.as_i64()).unwrap_or(0)
}

pub fn all(conn: &Connection) -> Result<Vec<Value>> {
    DEFS.iter()
        .map(|d| Ok(json!({"key": d.key, "value": get(conn, d.key)?, "default": (d.default)(), "desc": d.desc, "help": d.help})))
        .collect()
}

/// Parses `key=text; key=text` into pairs. Keys are single, non-space characters.
pub fn parse_quick_replies(s: &str) -> Result<Vec<(String, String)>> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, t) = p
                .split_once('=')
                .ok_or_else(|| anyhow!("quick reply '{p}' needs key=text"))?;
            let (k, t) = (k.trim(), t.trim());
            if k.chars().count() != 1 || k.chars().any(char::is_whitespace) || t.is_empty() {
                bail!("quick reply '{p}': the key is one character and the text is not empty");
            }
            Ok((k.to_string(), t.to_string()))
        })
        .collect()
}

pub fn quick_replies(conn: &Connection) -> Vec<(String, String)> {
    get(conn, "quick_replies")
        .ok()
        .and_then(|v| parse_quick_replies(v.as_str()?).ok())
        .unwrap_or_default()
}

fn validate(key: &str, value: &Value) -> Result<()> {
    let default = (def(key)?.default)();
    let same_type = matches!(
        (&default, value),
        (Value::Bool(_), Value::Bool(_))
            | (Value::String(_), Value::String(_))
            | (Value::Number(_), Value::Number(_))
    );
    if !same_type {
        let want = match default {
            Value::Bool(_) => "a boolean",
            Value::Number(_) => "a number",
            _ => "a string",
        };
        bail!("{key} expects {want}");
    }
    match key {
        "queue_width" => {
            let n = value
                .as_i64()
                .ok_or_else(|| anyhow!("queue_width expects a whole number"))?;
            if !(10..=90).contains(&n) {
                bail!("queue_width must be between 10 and 90");
            }
        }
        "queue_key" if value.as_str().is_some_and(|s| s.trim().is_empty()) => {
            bail!("queue_key must not be empty")
        }
        "quick_replies" => {
            parse_quick_replies(value.as_str().unwrap_or_default())?;
        }
        _ => {}
    }
    Ok(())
}

/// Sets a value; `null` resets to the default. Returns the stored value.
pub fn set(conn: &Connection, key: &str, value: &Value) -> Result<Value> {
    let d = def(key)?;
    if value.is_null() {
        conn.execute("DELETE FROM settings WHERE key=?", [key])?;
        return Ok((d.default)());
    }
    validate(key, value)?;
    conn.execute(
        "INSERT INTO settings VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, serde_json::to_string(value)?],
    )?;
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::temp_db;

    #[test]
    fn defaults_set_validate_and_reset() {
        let (_d, c) = temp_db();
        assert_eq!(get(&c, "queue_width").unwrap(), json!(38));
        assert_eq!(get(&c, "queue_key").unwrap(), json!("S-Escape"));
        assert!(get_bool(&c, "confirm_interrupt"));
        assert!(!get_bool(&c, "start_services_on_dispatch"));
        set(&c, "queue_width", &json!(50)).unwrap();
        assert_eq!(get_i64(&c, "queue_width"), 50);
        assert!(set(&c, "queue_width", &json!("50")).is_err());
        assert!(set(&c, "queue_width", &json!(5)).is_err());
        assert!(set(&c, "queue_width", &json!(40.5)).is_err());
        assert!(set(&c, "confirm_interrupt", &json!("no")).is_err());
        assert!(set(&c, "nope", &json!(1)).is_err());
        assert!(set(&c, "queue_key", &json!(" ")).is_err());
        assert_eq!(set(&c, "queue_width", &Value::Null).unwrap(), json!(38));
        assert_eq!(get_i64(&c, "queue_width"), 38);
    }

    #[test]
    fn quick_replies_parse_and_validate() {
        let (_d, c) = temp_db();
        let q = quick_replies(&c);
        assert_eq!(q[0], ("c".into(), "continue".into()));
        assert_eq!(q.len(), 5);
        assert!(set(&c, "quick_replies", &json!("ab=x")).is_err());
        assert!(set(&c, "quick_replies", &json!("a")).is_err());
        set(&c, "quick_replies", &json!("o=ok; n=no thanks")).unwrap();
        assert_eq!(
            quick_replies(&c),
            vec![("o".into(), "ok".into()), ("n".into(), "no thanks".into())]
        );
    }

    #[test]
    fn all_lists_every_setting_with_help() {
        let (_d, c) = temp_db();
        let v = all(&c).unwrap();
        assert_eq!(v.len(), 6);
        assert!(v
            .iter()
            .all(|s| !s["help"].as_str().unwrap().is_empty() && s["value"] == s["default"]));
    }
}
