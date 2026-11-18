//! The projects tab: a list of projects, and inside one its planned tickets by cluster, those in progress
//! and what is done.
use super::app::{App, Tab};
use super::widgets::{chips, fuzzy, input_line, str, tabs_line, Pick, Picker};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde_json::{json, Value};

/// A line in the project view; headers are not selectable.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PRow {
    pub head: bool,
    pub label: String,
    pub sub: String,
    pub key: String,
    pub session: Option<String>,
    pub status: String,
    pub body: String,
}

fn head(t: String) -> PRow {
    PRow { head: true, label: t, ..Default::default() }
}

/// The project overview as rows: PLANNED by cluster (unsorted last), IN PROGRESS with the session's state, DONE.
/// `filter` is a fuzzy match over label and details, applied locally.
pub fn project_rows(ov: &Value, filter: &str) -> Vec<PRow> {
    let none = vec![];
    let sessions = ov["sessions"].as_array().unwrap_or(&none);
    let ticket = |t: &Value, glyph: &str| {
        let s = sessions.iter().find(|s| s["key"] == t["key"]);
        let state = s
            .map(|s| match str(s, "state") {
                "needs" => " · needs you",
                "turn" => " · your turn",
                _ => " · working",
            })
            .unwrap_or("");
        PRow {
            head: false,
            label: format!("{glyph} {}  {}", str(t, "key"), str(t, "title")),
            sub: format!(
                "P{}{}{}",
                t["priority"],
                t["cluster"].as_str().map(|c| format!(" · ▸ {c}")).unwrap_or_default(),
                state
            ),
            key: str(t, "key").into(),
            session: s.map(|s| str(s, "session").to_string()),
            status: str(t, "status").into(),
            body: str(t, "body").into(),
        }
    };
    let keep = |r: &PRow| filter.is_empty() || fuzzy(filter, &format!("{} {}", r.label, r.sub));
    let list = |k: &str| ov[k].as_array().unwrap_or(&none).clone();
    let mut out = vec![];
    let planned: Vec<(Value, PRow)> =
        list("todo").into_iter().map(|t| (t.clone(), ticket(&t, "○"))).filter(|(_, r)| keep(r)).collect();
    out.push(head(format!("PLANNED {}", planned.len())));
    let mut cluster = String::from("\u{0}");
    for (t, r) in planned {
        let c = t["cluster"].as_str().unwrap_or("unsorted").to_string();
        if c != cluster {
            out.push(head(format!("  ▸ {c}")));
            cluster = c;
        }
        out.push(r);
    }
    for (title, key, glyph) in [("IN PROGRESS", "doing", "◐"), ("DONE", "done", "✓")] {
        let rows: Vec<PRow> = list(key).iter().map(|t| ticket(t, glyph)).filter(|r| keep(r)).collect();
        out.push(head(String::new()));
        out.push(head(format!("{title} {}", rows.len())));
        out.extend(rows);
    }
    out
}

/// The ticket as the home window's right pane shows it (ANSI-styled text).
pub fn ticket_text(t: &Value) -> String {
    let mut o = format!(
        "\n  \x1b[1m{}  {}\x1b[0m\n  \x1b[2m{} · P{}{}{}\x1b[0m\n\n",
        str(t, "key"),
        str(t, "title"),
        str(t, "status"),
        t["priority"],
        t["project"].as_str().map(|p| format!(" · ◆ {p}")).unwrap_or_default(),
        t["cluster"].as_str().map(|c| format!(" · ▸ {c}")).unwrap_or_default()
    );
    for l in str(t, "body").lines() {
        o += &format!("  {l}\n");
    }
    let links = t["links"].as_array().cloned().unwrap_or_default();
    if !links.is_empty() {
        o += "\n  \x1b[1mContext\x1b[0m\n";
        for l in links {
            o += &format!("  ▸ {}  \x1b[2m{}\x1b[0m\n", str(&l, "title"), str(&l, "why"));
        }
    }
    let sessions = t["sessions"].as_array().cloned().unwrap_or_default();
    if !sessions.is_empty() {
        o += "\n  \x1b[1mSessions\x1b[0m\n";
        for x in sessions {
            o += &format!(
                "  {}  {} · {}\n",
                str(&x, "session").get(..8).unwrap_or(""),
                str(&x, "state"),
                x["branch"].as_str().unwrap_or("?")
            );
        }
    }
    o + "\n  \x1b[2ms starts a session · g cluster · n new task\x1b[0m\n"
}

#[derive(Default)]
pub struct State {
    pub list: Vec<Value>,
    /// The project opened in the tab; `None` shows the project list. Kept across tab switches.
    pub open: Option<String>,
    pub plsel: usize,
    pub rows: Vec<PRow>,
    pub psel: usize,
    pub filter: String,
    /// `(is_filter, text)`; otherwise a new task's title.
    pub input: Option<(bool, String)>,
    pub clusters: Vec<String>,
    /// The last overview, re-filtered locally while typing.
    pub ov: Value,
}

impl State {
    pub fn cur(&self) -> Option<&PRow> {
        self.rows.get(self.psel).filter(|r| !r.head)
    }

    fn first_ticket(&self) -> usize {
        self.rows.iter().position(|r| !r.head).unwrap_or(0)
    }

    pub fn set_overview(&mut self, ov: Value) {
        self.clusters =
            ov["clusters"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from)).collect();
        self.ov = ov;
        self.refilter();
        self.psel = self.psel.min(self.rows.len().saturating_sub(1));
        if self.rows.get(self.psel).is_some_and(|r| r.head) {
            self.psel = self.first_ticket();
        }
    }

    fn refilter(&mut self) {
        self.rows = project_rows(&self.ov, &self.filter);
    }

    /// The next selectable row in direction `d`, skipping headers.
    fn step(&self, d: isize) -> usize {
        let mut i = self.psel as isize;
        loop {
            i += d;
            if i < 0 || i >= self.rows.len() as isize {
                return self.psel;
            }
            if !self.rows[i as usize].head {
                return i as usize;
            }
        }
    }

    fn open_project(&mut self, name: &str) {
        self.open = Some(name.into());
        self.psel = 0;
        self.filter.clear();
    }
}

impl App {
    pub(super) fn refresh_projects(&mut self) {
        let list =
            self.api.tool("project.list", json!({})).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
        self.projects.list = list;
        self.projects.plsel = self.projects.plsel.min(self.projects.list.len().saturating_sub(1));
        if let Some(name) = self.projects.open.clone() {
            let ov = self.api.tool("project.overview", json!({"project": name})).unwrap_or(Value::Null);
            self.projects.set_overview(ov);
        } else {
            self.projects.rows.clear();
        }
    }

    /// The live row of the selected ticket's session (the queue travels to its window).
    pub(super) fn project_focus(&self) -> Option<super::queue::Row> {
        let sid = self.projects.cur()?.session.as_ref()?;
        self.rows.iter().find(|r| &r.id == sid).cloned()
    }

    /// Typing a filter or a new task's title.
    pub(super) fn projects_input_key(&mut self, k: KeyEvent) {
        let p = &mut self.projects;
        let Some((is_filter, t)) = p.input.as_mut() else { return };
        let is_filter = *is_filter;
        match k.code {
            KeyCode::Esc => {
                if is_filter {
                    p.filter.clear();
                    p.refilter();
                }
                p.input = None;
                return;
            }
            KeyCode::Enter => {
                let title = std::mem::take(t);
                p.input = None;
                if !is_filter && !title.is_empty() {
                    if let Some(project) = p.open.clone() {
                        self.flash = match self.api.tool("ticket.add", json!({"project": project, "title": title})) {
                            Ok(v) => format!("added {}", str(&v, "key")),
                            Err(e) => e,
                        };
                    }
                }
                return;
            }
            KeyCode::Backspace => {
                t.pop();
            }
            KeyCode::Char(c) => t.push(c),
            _ => {}
        }
        if is_filter {
            p.filter = t.clone();
            p.refilter();
            p.psel = p.first_ticket();
        }
    }

    /// Tab-specific keys; false when the key is not for this tab.
    pub(super) fn projects_key(&mut self, k: KeyEvent) -> bool {
        if self.projects.open.is_none() {
            let p = &mut self.projects;
            match k.code {
                KeyCode::Char('j') | KeyCode::Down => p.plsel = (p.plsel + 1).min(p.list.len().saturating_sub(1)),
                KeyCode::Char('k') | KeyCode::Up => p.plsel = p.plsel.saturating_sub(1),
                KeyCode::Enter => {
                    if let Some(name) = p.list.get(p.plsel).map(|x| str(x, "name").to_string()) {
                        p.open_project(&name);
                    }
                }
                _ => return false,
            }
            return true;
        }
        let cur = self.projects.cur().cloned();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.projects.psel = self.projects.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.projects.psel = self.projects.step(-1),
            KeyCode::Esc => {
                self.projects.open = None;
                self.projects.filter.clear();
            }
            KeyCode::Char('o') => {
                let items = self
                    .projects
                    .list
                    .iter()
                    .map(|p| {
                        (
                            format!("◆ {}  ({} planned, {} in progress)", str(p, "name"), p["planned"], p["doing"]),
                            str(p, "name").to_string(),
                            String::new(),
                        )
                    })
                    .collect();
                self.picker = Some(Picker::new(Pick::OpenProject, "Open project".into(), items));
            }
            KeyCode::Char('/') => self.projects.input = Some((true, self.projects.filter.clone())),
            KeyCode::Char('n') => self.projects.input = Some((false, String::new())),
            KeyCode::Char('g') => {
                if let Some(r) = &cur {
                    let items =
                        self.projects.clusters.iter().map(|c| (format!("▸ {c}"), c.clone(), String::new())).collect();
                    let mut p = Picker::new(Pick::Cluster, format!("Cluster for {} (type to create)", r.key), items);
                    p.key = r.key.clone();
                    self.picker = Some(p);
                }
            }
            KeyCode::Char('H') => {
                if let Some(project) = self.projects.open.clone() {
                    self.flash = match self.api.tool("project.housekeep", json!({"project": project})) {
                        Ok(v) => format!("Claude is tidying ◆ {project} in {}", str(&v, "window")),
                        Err(e) => e,
                    };
                }
            }
            KeyCode::Char('s') => {
                if let Some(r) = cur.filter(|r| r.status == "todo" || r.status == "backlog") {
                    self.start_ticket(&r.key);
                }
            }
            KeyCode::Enter => match cur.and_then(|r| r.session) {
                Some(s) => {
                    self.sel_id = s;
                    self.tab = Tab::Queue;
                }
                None => self.flash = "no live session for this ticket (s starts one)".into(),
            },
            _ => return false,
        }
        true
    }

    pub(super) fn draw_projects(&self, f: &mut Frame, left: Rect, right: Rect) {
        let p = &self.projects;
        let tabs = tabs_line(self.tab as usize);
        let mut ls = ListState::default();
        let items: Vec<ListItem> = if p.open.is_none() {
            let mut items = vec![
                ListItem::new(tabs),
                ListItem::new(Line::from(format!("PROJECTS {}", p.list.len())).bold()),
                ListItem::new(""),
            ];
            if p.list.is_empty() {
                items.push(ListItem::new(Line::from(" none yet: p on a ticket assigns one").dim()));
            }
            for x in &p.list {
                items.push(ListItem::new(vec![
                    Line::from(format!(" ◆ {}", str(x, "name"))).bold(),
                    Line::from(format!(
                        "     ○ {} planned · ◐ {} in progress · ✓ {} done{}",
                        x["planned"],
                        x["doing"],
                        x["done"],
                        x["palaver"].as_str().map(|v| format!(" · Palaver: {v}")).unwrap_or_default()
                    ))
                    .dim(),
                ]));
            }
            ls.select(if p.list.is_empty() { None } else { Some(p.plsel + 3) });
            items
        } else {
            let name = p.open.as_deref().unwrap_or("");
            let title =
                format!("◆ {name}{}", if p.filter.is_empty() { String::new() } else { format!("   / {}", p.filter) });
            let mut items = vec![ListItem::new(tabs), ListItem::new(Line::from(title).bold()), ListItem::new("")];
            for (i, r) in p.rows.iter().enumerate() {
                items.push(if r.head {
                    ListItem::new(Line::from(r.label.clone()).bold())
                } else {
                    let style =
                        if r.status == "done" { Style::new().add_modifier(Modifier::DIM) } else { Style::new() };
                    let mut lines = vec![
                        Line::from(format!(" {}", r.label)).style(style),
                        Line::from(format!("     {}", r.sub)).dim(),
                    ];
                    if i == p.psel {
                        let mut h: Vec<(&str, String)> = vec![];
                        if r.session.is_some() {
                            h.push(("⏎", "open session".into()));
                        }
                        if r.status == "todo" || r.status == "backlog" {
                            h.push(("s", "start".into()));
                        }
                        if r.status != "done" {
                            h.push(("g", "cluster".into()));
                        }
                        lines.push(chips(&h));
                    }
                    ListItem::new(lines)
                });
            }
            ls.select(p.cur().map(|_| p.psel + 3));
            items
        };
        f.render_stateful_widget(
            List::new(items)
                .block(Block::new().borders(Borders::BOTTOM))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            left,
            &mut ls,
        );
        if let Some(r) = p.cur() {
            let mut lines = vec![
                Line::from(r.label.clone()).bold(),
                Line::from(format!("{} · {}", r.status, r.sub)).dim(),
                Line::from(""),
            ];
            lines.extend(r.body.lines().map(|l| Line::from(l.to_string())));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), right);
        }
    }

    pub(super) fn projects_input_line(&self) -> Option<Line<'static>> {
        match &self.projects.input {
            Some((true, t)) => Some(input_line("filter", t)),
            Some((false, t)) => Some(input_line("new task", t)),
            None => None,
        }
    }

    pub(super) fn projects_help(&self) -> &'static str {
        if self.projects.open.is_none() {
            " ⇥ settings · j/k move · ⏎ open project · , settings · q leave"
        } else {
            " ⇥ settings · esc projects · o switch project · j/k move · / filter · n new task · g cluster · H let Claude housekeep · s start · ⏎ open session · q leave"
        }
    }
}

#[cfg(test)]
pub fn sample_overview() -> Value {
    json!({
        "project": {"name": "Demo"},
        "todo": [
            {"key": "DEM-2", "title": "Ready one", "status": "todo", "priority": 1, "cluster": "auth", "body": "do it"},
            {"key": "DEM-3", "title": "Polish", "status": "backlog", "priority": 3, "cluster": null, "body": ""},
        ],
        "doing": [{"key": "DEM-1", "title": "Login", "status": "doing", "priority": 2, "cluster": "auth", "body": "b"}],
        "done": [{"key": "DEM-0", "title": "Scaffold", "status": "done", "priority": 2, "cluster": null, "body": ""}],
        "sessions": [{"session": "s1", "key": "DEM-1", "state": "needs"}],
        "clusters": ["auth"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_groups_planned_by_cluster_with_unsorted_last() {
        let rows = project_rows(&sample_overview(), "");
        let lines: Vec<String> =
            rows.iter().map(|r| format!("{}{}", if r.head { "# " } else { "  " }, r.label)).collect();
        assert_eq!(
            lines,
            [
                "# PLANNED 2",
                "#   ▸ auth",
                "  ○ DEM-2  Ready one",
                "#   ▸ unsorted",
                "  ○ DEM-3  Polish",
                "# ",
                "# IN PROGRESS 1",
                "  ◐ DEM-1  Login",
                "# ",
                "# DONE 1",
                "  ✓ DEM-0  Scaffold",
            ]
        );
        let doing = &rows[7];
        assert_eq!(doing.sub, "P2 · ▸ auth · needs you");
        assert_eq!(doing.session.as_deref(), Some("s1"));
    }

    #[test]
    fn filter_is_fuzzy_and_local() {
        let rows = project_rows(&sample_overview(), "plsh");
        let tickets: Vec<&str> = rows.iter().filter(|r| !r.head).map(|r| r.key.as_str()).collect();
        assert_eq!(tickets, ["DEM-3"]);
        assert_eq!(rows[0].label, "PLANNED 1");
        assert!(rows.iter().any(|r| r.label == "DONE 0"));
    }

    #[test]
    fn ticket_text_has_status_body_context_and_sessions() {
        let t = json!({"key": "DEM-2", "title": "Ready", "status": "todo", "priority": 1, "project": "Demo", "cluster": "auth", "body": "l1\nl2",
            "links": [{"title": "Kickoff", "why": "scope"}], "sessions": [{"session": "abcdef0123", "state": "turn", "branch": "feat/x"}]});
        let s = ticket_text(&t);
        assert!(s.contains("todo · P1 · ◆ Demo · ▸ auth"));
        assert!(s.contains("  l1\n  l2\n"));
        assert!(s.contains("▸ Kickoff"));
        assert!(s.contains("abcdef01  turn · feat/x"));
        assert!(s.ends_with("n new task\x1b[0m\n"));
    }
}
