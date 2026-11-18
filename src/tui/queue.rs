//! The queue tab: sessions that need you, your turn, working, and ready tickets.
use super::widgets::{ago, chips, str};
use crate::util::ms_from_iso;
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::ListItem,
};
use serde_json::Value;

pub const NEEDS: usize = 0;
pub const TURN: usize = 1;
pub const WORKING: usize = 2;
/// In-progress tickets whose window exists but whose Claude session is not live.
pub const ENDED: usize = 3;
pub const READY: usize = 4;
pub const SECTIONS: [(&str, Color); 5] = [
    ("NEEDS YOU", Color::Red),
    ("YOUR TURN", Color::Blue),
    ("WORKING", Color::Green),
    ("ENDED", Color::DarkGray),
    ("READY", Color::Reset),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub section: usize,
    /// Session id, or the ticket key for ready rows.
    pub id: String,
    pub label: String,
    pub sub: String,
    /// `session:window` of the pane, or "desktop / no tmux".
    pub place: String,
    /// Epoch seconds the state (or the ticket) began.
    pub since: i64,
    pub msg: String,
    /// The session's pane exists on some tmux server (replies work).
    pub pane: bool,
    /// The pane id on the cockpit's own server (`%12`), the only ones that can be shown next to the queue.
    pub cockpit_pane: Option<String>,
    /// Ticket key, empty for sessions without a ticket.
    pub key: String,
    pub project: Option<String>,
}

/// The row of a ticket's window whose Claude session is not live (`pane` is the cockpit pane ref).
pub fn window_row(key: &str, label: &str, pane: &str) -> Row {
    Row {
        section: ENDED,
        id: key.into(),
        label: label.into(),
        sub: format!("cockpit:{key}"),
        place: format!("cockpit:{key}"),
        since: 0,
        msg: String::new(),
        pane: false,
        cockpit_pane: pane.strip_prefix("cockpit:").map(String::from),
        key: key.into(),
        project: None,
    }
}

fn secs(iso: &str) -> i64 {
    ms_from_iso(iso).map_or(0, |m| m / 1000)
}

fn proj_suffix(p: Option<&str>) -> String {
    p.map(|p| format!(" · ◆ {p}")).unwrap_or_default()
}

/// The queue's rows from `/api/state`: sessions by section, then ready tickets in rank order.
pub fn rows(st: &Value) -> Vec<Row> {
    let empty = vec![];
    let tickets = st["tickets"].as_array().unwrap_or(&empty);
    let mut out = vec![];
    for s in st["sessions"].as_array().unwrap_or(&empty) {
        let section = match str(s, "state") {
            "needs" => NEEDS,
            "turn" => TURN,
            "working" => WORKING,
            _ => continue,
        };
        let pane = str(s, "tmux_pane");
        // a session whose pane is gone or whose end was recorded is over
        if s["ended"].is_string() || (!pane.is_empty() && st["where"].get(pane).is_none()) {
            continue;
        }
        let id = str(s, "id");
        let ticket = tickets.iter().filter(|t| str(t, "session") == id).max_by_key(|t| str(t, "status") == "doing");
        let branch = Some(s["git"]["branch"].as_str().unwrap_or("")).filter(|b| !b.is_empty()).unwrap_or("?");
        let label = match ticket {
            Some(t) => format!("{}  {}", str(t, "key"), str(t, "title")),
            // sessions without a ticket go by their cockpit window (Demo-housekeeping, DEM-7-ctx), else repo · branch
            None => st["where"][pane]
                .as_str()
                .filter(|_| pane.starts_with("cockpit:"))
                .and_then(|w| w.split_once(':'))
                .map(|(_, w)| w.to_string())
                .unwrap_or_else(|| format!("{} · {}", str(s, "repo"), branch)),
        };
        let project = ticket.and_then(|t| t["project"].as_str()).filter(|p| !p.is_empty()).map(String::from);
        let place = st["where"][pane].as_str().map(String::from).unwrap_or_else(|| "desktop / no tmux".into());
        let msg = if section == WORKING {
            let trail: Value = serde_json::from_str(str(s, "trail")).unwrap_or(Value::Null);
            let mut t: Vec<String> =
                trail.as_array().unwrap_or(&empty).iter().map(|x| format!("› {}", str(x, "what"))).collect();
            t.insert(0, str(s, "state_detail").trim().to_string());
            t.join("\n")
        } else {
            str(s, "last_message").to_string()
        };
        out.push(Row {
            section,
            id: id.into(),
            label,
            sub: format!("{} · {} · {}{}", str(s, "repo"), branch, place, proj_suffix(project.as_deref())),
            place,
            since: secs(str(s, "state_at")),
            msg,
            pane: st["where"].get(pane).is_some(),
            cockpit_pane: pane.strip_prefix("cockpit:").filter(|_| st["where"].get(pane).is_some()).map(String::from),
            key: ticket.map(|t| str(t, "key").to_string()).unwrap_or_default(),
            project,
        });
    }
    // doing tickets that have a window but no live session: the window's shell can be reopened or Claude restarted
    for t in tickets.iter().filter(|t| str(t, "status") == "doing") {
        let key = str(t, "key");
        let Some(w) = st["windows"].as_array().unwrap_or(&empty).iter().find(|w| str(w, "name") == key) else {
            continue;
        };
        if out.iter().any(|r| r.key == key) {
            continue;
        }
        let project = t["project"].as_str().filter(|p| !p.is_empty()).map(String::from);
        out.push(Row {
            sub: format!("{} · cockpit:{key}{}", str(t, "repo"), proj_suffix(project.as_deref())),
            since: secs(str(t, "updated")),
            msg: str(t, "body").into(),
            project,
            ..window_row(key, &format!("{key}  {}", str(t, "title")), str(w, "pane"))
        });
    }
    for k in st["ready"].as_array().unwrap_or(&empty) {
        if let Some(t) = tickets.iter().find(|t| t["key"] == *k) {
            let project = t["project"].as_str().filter(|p| !p.is_empty()).map(String::from);
            out.push(Row {
                section: READY,
                id: str(t, "key").into(),
                label: format!("{}  {}", str(t, "key"), str(t, "title")),
                sub: format!("{} · P{}{}", str(t, "repo"), t["priority"], proj_suffix(project.as_deref())),
                place: String::new(),
                since: secs(str(t, "created")),
                msg: str(t, "body").into(),
                pane: false,
                cockpit_pane: None,
                key: str(t, "key").into(),
                project,
            });
        }
    }
    // waiting rows: longest wait first; working: most recent first; ready keeps rank order
    out.sort_by_key(|r| {
        (
            r.section,
            if r.section < WORKING {
                r.since
            } else if r.section == WORKING || r.section == ENDED {
                -r.since
            } else {
                0
            },
        )
    });
    out
}

/// "c=continue; y=yes" as `[('c', "continue"), ('y', "yes")]`.
pub fn quick_replies(settings: &[Value]) -> Vec<(char, String)> {
    let raw =
        settings.iter().find(|s| str(s, "key") == "quick_replies").and_then(|s| s["value"].as_str()).unwrap_or("");
    crate::settings::parse_quick_replies(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, t)| k.chars().next().map(|c| (c, t)))
        .collect()
}

/// The hotkeys under the selected row.
pub fn row_hints(r: &Row) -> Vec<(&'static str, String)> {
    let mut h: Vec<(&'static str, String)> = match r.section {
        NEEDS => vec![("1-4", "pick".into()), ("a", "accept".into()), ("r", "reply".into()), ("x", "interrupt".into())],
        TURN => vec![("r", "reply".into())],
        WORKING => vec![("x", "interrupt".into())],
        ENDED => vec![("s", "restart session".into()), ("⏎", "open window".into())],
        _ => vec![
            ("s", "start".into()),
            ("S", "start in background".into()),
            ("p", "project".into()),
            ("l", "meeting".into()),
        ],
    };
    if r.section < READY && r.pane {
        h.insert(0, ("⏎", "attach".into()));
    }
    if !r.key.is_empty() {
        h.push(("C", "close out".into()));
    }
    h
}

/// The same hints for the session pane's border, as tmux styles: keys bold blue.
pub fn border_hints(r: &Row, replies: &[(char, String)]) -> String {
    let k = |key: &str, what: &str| format!("#[fg=blue,bold]{key}#[default] {what}   ");
    let mut h = String::new();
    match r.section {
        NEEDS => {
            h += &k("1-4", "pick");
            h += &k("a", "accept");
            h += &k("x", "interrupt");
        }
        WORKING => h += &k("x", "interrupt"),
        ENDED => h += &k("s", "restart session"),
        _ => {}
    }
    if r.section < WORKING {
        for (c, t) in replies {
            h += &k(&c.to_string(), t);
        }
        h += &k("r", "reply");
    }
    h + &k("⏎", "attach")
}

/// What `@tk_label` holds while the queue is focused: the label in bold, then the hints. `#` is doubled so
/// tmux draws it literally.
pub fn window_label(label: &str, hints: &str) -> String {
    format!("#[bold]{}#[default]  │  {hints}", label.replace('#', "##"))
}

/// List items for the queue and the list index of the selection.
pub fn list_items(rows: &[Row], sel_id: &str, now: i64) -> (Vec<ListItem<'static>>, Option<usize>) {
    let mut items = vec![];
    let mut selected = None;
    for (si, (name, color)) in SECTIONS.iter().enumerate() {
        let rs: Vec<&Row> = rows.iter().filter(|r| r.section == si).collect();
        if rs.is_empty() {
            continue;
        }
        items.push(ListItem::new(
            Line::from(format!("{name} {}", rs.len())).style(Style::new().fg(*color).add_modifier(Modifier::BOLD)),
        ));
        for r in rs {
            let wait = if si < READY { ago(now - r.since) } else { String::new() };
            let mut lines = vec![
                Line::from(vec![Span::raw(format!(" {} ", r.label)), Span::styled(wait, Style::new().fg(*color))]),
                Line::from(format!("   {}", r.sub)).dim(),
            ];
            if r.id == sel_id {
                lines.push(chips(&row_hints(r)));
                selected = Some(items.len());
            }
            items.push(ListItem::new(lines));
        }
        items.push(ListItem::new(""));
    }
    (items, selected)
}

/// The block under the list: who, how long, project and Palaver context, then the message or recent tool calls.
pub fn detail(r: &Row, context: &[String], now: i64) -> Vec<Line<'static>> {
    let (name, color) = SECTIONS[r.section];
    let mut lines = vec![
        Line::from(r.label.clone()).bold(),
        Line::from(vec![
            Span::styled(format!("{name} "), Style::new().fg(color)),
            Span::raw(format!("{} · {}", ago(now - r.since), r.sub)).dim(),
        ]),
        Line::from(""),
    ];
    lines.extend(context.iter().map(|l| Line::from(l.clone()).magenta()));
    if !context.is_empty() {
        lines.push(Line::from(""));
    }
    lines.extend(r.msg.lines().map(|l| Line::from(l.to_string())));
    lines
}

/// Two tickets and a handful of sessions, for tests of the queue and the app.
#[cfg(test)]
pub fn sample_state() -> Value {
    serde_json::json!({
            "tickets": [
                {"key": "DEM-1", "title": "Login", "status": "doing", "session": "s1", "project": "Demo", "repo": "demo", "priority": 2, "created": "2026-11-18T08:00:00Z", "body": ""},
                {"key": "DEM-2", "title": "Ready one", "status": "todo", "session": null, "project": null, "repo": "demo", "priority": 1, "created": "2026-11-18T07:00:00Z", "body": "do it"},
            ],
            "sessions": [
                {"id": "s1", "state": "needs", "state_at": "2026-11-18T09:00:00Z", "repo": "demo", "tmux_pane": "cockpit:%7", "last_message": "which db?", "git": {"branch": "feat/x"}},
                {"id": "s2", "state": "working", "state_at": "2026-11-18T09:30:00Z", "repo": "demo", "tmux_pane": "cockpit:%8", "state_detail": "Bash", "trail": "[{\"what\":\"Edit a.rs\"}]", "git": {"branch": ""}},
                {"id": "s3", "state": "turn", "state_at": "2026-11-18T09:10:00Z", "repo": "demo", "tmux_pane": "%3", "git": {"branch": "main"}},
                {"id": "s4", "state": "turn", "state_at": "2026-11-18T09:10:00Z", "repo": "demo", "tmux_pane": "cockpit:%99", "git": {}},
                {"id": "s5", "state": "ended", "state_at": "2026-11-18T09:10:00Z", "repo": "demo", "tmux_pane": "cockpit:%7"},
            ],
            "where": {"cockpit:%7": "cockpit:DEM-1", "cockpit:%8": "cockpit:Demo-housekeeping", "%3": "main:zsh"},
            "ready": ["DEM-2"],
    })
}

/// `sample_state` plus DEM-3: in progress, its session ended, its window still there (and DEM-1's window).
#[cfg(test)]
pub fn ended_state() -> Value {
    let mut st = sample_state();
    st["tickets"].as_array_mut().unwrap().push(serde_json::json!(
        {"key": "DEM-3", "title": "Dropped", "status": "doing", "session": "s5", "project": null, "repo": "demo", "priority": 2, "created": "2026-11-18T08:00:00Z", "updated": "2026-11-18T08:30:00Z", "body": ""}));
    st["windows"] = serde_json::json!([
        {"name": "DEM-1", "pane": "cockpit:%7", "claude": true},
        {"name": "DEM-3", "pane": "cockpit:%11", "claude": false},
    ]);
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> Value {
        sample_state()
    }

    #[test]
    fn rows_are_sectioned_labelled_and_ordered() {
        let r = rows(&state());
        let ids: Vec<&str> = r.iter().map(|r| r.id.as_str()).collect();
        // s4's pane is gone, s5 ended
        assert_eq!(ids, ["s1", "s3", "s2", "DEM-2"]);
        assert_eq!(r[0].label, "DEM-1  Login");
        assert_eq!(r[0].sub, "demo · feat/x · cockpit:DEM-1 · ◆ Demo");
        assert_eq!(r[0].cockpit_pane.as_deref(), Some("%7"));
        // no ticket: window name for cockpit panes, repo · branch elsewhere
        assert_eq!(r[2].label, "Demo-housekeeping");
        assert_eq!(r[1].label, "demo · main");
        assert!(r[1].pane && r[1].cockpit_pane.is_none());
        assert_eq!(r[2].msg, "Bash\n› Edit a.rs");
        assert_eq!(r[3].sub, "demo · P1");
        assert_eq!(r[3].msg, "do it");
    }

    #[test]
    fn hints_follow_the_state() {
        let r = rows(&state());
        let keys = |r: &Row| row_hints(r).iter().map(|(k, _)| *k).collect::<Vec<_>>();
        assert_eq!(keys(&r[0]), ["⏎", "1-4", "a", "r", "x", "C"]);
        assert_eq!(keys(&r[1]), ["⏎", "r"]);
        assert_eq!(keys(&r[2]), ["⏎", "x"]);
        assert_eq!(keys(&r[3]), ["s", "S", "p", "l", "C"]);
        let q = vec![('c', "continue".to_string())];
        let strip = |s: String| s.replace("#[fg=blue,bold]", "<").replace("#[default]", ">");
        assert_eq!(
            strip(border_hints(&r[0], &q)),
            "<1-4> pick   <a> accept   <x> interrupt   <c> continue   <r> reply   <⏎> attach   "
        );
        assert_eq!(strip(border_hints(&r[1], &q)), "<c> continue   <r> reply   <⏎> attach   ");
        assert_eq!(strip(border_hints(&r[2], &q)), "<x> interrupt   <⏎> attach   ");
    }

    #[test]
    fn labels_escape_hashes() {
        assert_eq!(window_label("DEM-1  fix #12", "h"), "#[bold]DEM-1  fix ##12#[default]  │  h");
    }

    #[test]
    fn quick_replies_parse_from_settings() {
        let s = vec![json!({"key": "quick_replies", "value": "c=continue; y = yes ;bad; m=merge it"})];
        // a malformed entry invalidates nothing the backend accepted; here the whole string is refused
        assert!(quick_replies(&s).is_empty());
        let s = vec![json!({"key": "quick_replies", "value": "c=continue; y = yes; m=merge it"})];
        assert_eq!(
            quick_replies(&s),
            [('c', "continue".to_string()), ('y', "yes".to_string()), ('m', "merge it".to_string())]
        );
        assert!(quick_replies(&[]).is_empty());
    }

    #[test]
    fn doing_tickets_with_a_window_but_no_live_session_are_ended() {
        let r = rows(&ended_state());
        let ids: Vec<&str> = r.iter().map(|r| r.id.as_str()).collect();
        // DEM-1 has a window too, but its session is live: it stays a session row
        assert_eq!(ids, ["s1", "s3", "s2", "DEM-3", "DEM-2"]);
        let e = &r[3];
        assert_eq!((e.section, e.key.as_str(), e.cockpit_pane.as_deref()), (ENDED, "DEM-3", Some("%11")));
        assert_eq!((e.label.as_str(), e.sub.as_str()), ("DEM-3  Dropped", "demo · cockpit:DEM-3"));
        assert!(!e.pane, "no replies go to an ended window");
        let hints: Vec<_> = row_hints(e).into_iter().map(|(k, a)| format!("[{k}] {a}")).collect();
        assert_eq!(hints, ["[s] restart session", "[⏎] open window", "[C] close out"]);
        // a doing ticket without a window is not listed, nor is a window of a ticket that is not in progress
        let mut st = ended_state();
        st["windows"] = json!([{"name": "DEM-2", "pane": "cockpit:%12", "claude": false}]);
        assert!(rows(&st).iter().all(|r| r.section != ENDED));
        let strip = |s: String| s.replace("#[fg=blue,bold]", "<").replace("#[default]", ">");
        assert_eq!(strip(border_hints(e, &[])), "<s> restart session   <⏎> attach   ");
    }
}
