//! The queue's state, its refresh tick and key routing.
use super::api::{is_down, Api};
use super::bindings;
use super::projects;
use super::queue::{self, Row, ENDED, READY};
use super::settings;
use super::travel::Travel;
use super::widgets::{input_line, picker_key, render_picker, str, tabs_line, Pick, Picker, PickerKey};
use crate::tmux::Tmux;
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde_json::{json, Value};

/// A y/n question in the footer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Confirm {
    Reload,
    Update,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tab {
    Queue,
    Projects,
    Settings,
}

const TAB_ORDER: [Tab; 3] = [Tab::Queue, Tab::Projects, Tab::Settings];

pub struct App {
    pub api: Api,
    pub tmux: Tmux,
    pub travel: Option<Travel>,
    me: String,
    bound_key: String,
    pub tab: Tab,
    pub rows: Vec<Row>,
    /// The cockpit session's ticket windows (`name`, `pane`, `claude`).
    pub windows: Vec<Value>,
    pub sel_id: String,
    pub settings: Vec<Value>,
    /// Project and linked context of the selected ticket.
    pub context: Vec<String>,
    pub flash: String,
    /// The reply line (`r`).
    pub input: Option<String>,
    /// `!`: the "improve the cockpit" line.
    pub fb_input: Option<String>,
    /// Close now: `(ticket key, summary)`.
    pub summary_input: Option<(String, String)>,
    pub confirm: Option<Confirm>,
    /// The daemon's update check.
    pub update: Value,
    pub projects: projects::State,
    pub settings_state: settings::State,
    pub picker: Option<Picker>,
    pub show_help: bool,
    /// Set while the daemon does not answer.
    pub down: Option<String>,
    /// The daemon is another build than this queue: shown as a one-line notice.
    pub stale_daemon: bool,
    /// `(ticket key, since)`: a session was just started for it; select its row when it appears.
    pending: Option<(String, i64)>,
    /// Refresh on the next loop turn (after an action).
    pub refresh: bool,
    pub quit: bool,
    /// Seconds since the epoch, for waiting times.
    pub now: i64,
}

pub fn now_secs() -> i64 {
    crate::util::now_ms() / 1000
}

impl App {
    pub fn new(api: Api, tmux: Tmux, travel: Option<Travel>, me: &str) -> Self {
        App {
            api,
            tmux,
            travel,
            me: me.into(),
            bound_key: String::new(),
            tab: Tab::Queue,
            rows: vec![],
            windows: vec![],
            sel_id: String::new(),
            settings: vec![],
            context: vec![],
            flash: String::new(),
            input: None,
            fb_input: None,
            summary_input: None,
            confirm: None,
            update: Value::Null,
            projects: Default::default(),
            settings_state: Default::default(),
            picker: None,
            show_help: false,
            down: None,
            stale_daemon: false,
            pending: None,
            refresh: true,
            quit: false,
            now: now_secs(),
        }
    }

    fn idx(&self) -> usize {
        self.rows.iter().position(|r| r.id == self.sel_id).unwrap_or(0).min(self.rows.len().saturating_sub(1))
    }

    pub fn cur(&self) -> Option<&Row> {
        self.rows.get(self.idx())
    }

    fn setting(&self, key: &str) -> Value {
        self.settings.iter().find(|s| str(s, "key") == key).map(|s| s["value"].clone()).unwrap_or(Value::Null)
    }

    pub fn replies(&self) -> Vec<(char, String)> {
        queue::quick_replies(&self.settings)
    }

    fn tool(&self, name: &str, args: Value) -> Result<Value, String> {
        self.api.tool(name, args)
    }

    /// Takes the daemon's state: rows, and the selection kept (or moved to what the bar asked for).
    pub fn load_state(&mut self, st: &Value) {
        self.rows = queue::rows(st);
        self.windows = st["windows"].as_array().cloned().unwrap_or_default();
        self.resolve_pending();
        let i = self.idx();
        if self.pending.is_none() {
            self.sel_id = self.rows.get(i).map(|r| r.id.clone()).unwrap_or_default();
        }
    }

    /// One refresh: data from the daemon, tmux bindings, where the queue sits.
    pub fn tick(&mut self) {
        self.now = now_secs();
        match self.api.get("/api/state") {
            Ok(st) => {
                self.down = None;
                if let Ok(h) = self.api.get("/api/health") {
                    self.stale_daemon = h["build"].as_str() != Some(crate::util::build_id().as_str());
                }
                self.load_state(&st);
                if let Ok(s) = self.tool("settings.get", json!({})) {
                    self.settings = s.as_array().cloned().unwrap_or_default();
                }
                if let Ok(u) = self.tool("cockpit.update_status", json!({})) {
                    self.update = u;
                }
            }
            Err(e) if is_down(&e) => self.down = Some(e),
            Err(e) => self.flash = e,
        }
        if self.down.is_none() {
            self.refresh_context();
            if self.tab == Tab::Projects {
                self.refresh_projects();
            }
        }
        if self.travel.is_some() {
            self.tick_tmux();
        }
    }

    fn tick_tmux(&mut self) {
        let key = bindings::queue_key(&self.settings);
        self.bound_key = bindings::assert_keys(&self.tmux, &self.me, &key, &self.bound_key);
        if bindings::sync_confirm(&self.tmux, bindings::confirm_setting(&self.settings)) {
            let _ = self.tool("settings.set", json!({"key": "confirm_interrupt", "value": false}));
        }
        let Some(mut travel) = self.travel.take() else { return };
        if let Some(id) = travel.pending_select() {
            self.sel_id = id;
        }
        let focus = self.focus();
        let width = self.setting("queue_width").as_i64().unwrap_or(38);
        let focus_pane = focus.as_ref().is_some_and(|r| r.cockpit_pane.is_some());
        travel.show_slot(&self.slot_key(focus_pane), || self.slot_text());
        let out =
            travel.sync(focus.as_ref(), &self.rows, &self.sel_id, self.tab == Tab::Projects, width, &self.replies());
        if let Some(id) = out.select {
            self.sel_id = id;
        }
        self.refresh |= out.refresh;
        self.travel = Some(travel);
    }

    /// The row whose window the queue should sit in.
    /// In the projects tab the selected ticket decides: its live session's window, else the ticket itself in the home window.
    fn focus(&self) -> Option<Row> {
        match self.tab {
            Tab::Queue => self.pending_focus().or_else(|| self.cur().cloned()),
            Tab::Projects => self.project_focus(),
            Tab::Settings => None,
        }
    }

    /// Identifies what the home window's right pane shows, so it is redrawn only when that changes.
    fn slot_key(&self, focus_pane: bool) -> String {
        match self.tab {
            Tab::Projects if !focus_pane => self.projects.cur().map(|p| p.key.clone()).unwrap_or_default(),
            Tab::Settings => self.settings_slot_key(),
            _ => String::new(),
        }
    }

    fn slot_text(&self) -> String {
        let key = self.slot_key(false);
        if self.tab == Tab::Settings {
            return self.settings_slot_text();
        }
        if self.tab != Tab::Projects || key.is_empty() {
            return "\n  no live tmux session selected\n".to_string();
        }
        match self.tool("ticket.get", json!({"key": key})) {
            Ok(t) => projects::ticket_text(&t),
            Err(e) => format!("\n  {e}\n"),
        }
    }

    fn refresh_context(&mut self) {
        self.context = match self.cur().filter(|r| !r.key.is_empty()).cloned() {
            Some(r) => {
                let mut c = vec![];
                if let Some(p) = &r.project {
                    let projects = self.tool("project.list", json!({})).unwrap_or(json!([]));
                    c.extend(projects.as_array().into_iter().flatten().filter(|x| str(x, "name") == p).map(|x| {
                        format!("◆ {} · Palaver: {}", p, x["palaver"].as_str().unwrap_or("not linked (l to link)"))
                    }));
                }
                let links = self.tool("ticket.links", json!({"key": r.key})).unwrap_or(json!([]));
                c.extend(
                    links
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|l| format!("  ▸ {}  {}", str(l, "title"), str(l, "why"))),
                );
                c
            }
            None => vec![],
        };
    }

    /// Q and shutdown: the queue goes home and the slot closes.
    pub fn shutdown(&mut self) {
        if let Some(t) = &self.travel {
            t.stop();
        }
    }

    // ---- keys ------------------------------------------------------------------------------------------------

    fn send(&self, body: Value) -> String {
        match self.cur() {
            Some(r) if r.section == READY => String::new(),
            Some(r) if r.section == ENDED => format!("{} has no live session: s restarts it", r.key),
            Some(r) if r.pane => self.api.send(&r.id, body).err().unwrap_or_default(),
            Some(_) => "not in tmux: view only".into(),
            None => String::new(),
        }
    }

    fn send_reply(&mut self, text: &str) {
        self.flash = self.send(json!({"text": text}));
        if self.flash.is_empty() && self.cur().is_some_and(|r| r.pane && r.section < ENDED) {
            self.flash = format!("sent \"{text}\"");
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        self.flash.clear();
        self.pending = None; // the user took over
        self.refresh = true; // refresh right after an action
        if self.picker.is_some() {
            return self.picker_key(k);
        }
        if let Some(t) = self.input.as_mut() {
            match k.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    t.pop();
                }
                KeyCode::Char(c) => t.push(c),
                KeyCode::Enter => {
                    let text = std::mem::take(t);
                    self.input = None;
                    if !text.is_empty() {
                        self.send_reply(&text);
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(t) = self.fb_input.as_mut() {
            match k.code {
                KeyCode::Esc => self.fb_input = None,
                KeyCode::Backspace => {
                    t.pop();
                }
                KeyCode::Char(c) => t.push(c),
                KeyCode::Enter => {
                    let text = std::mem::take(t);
                    self.fb_input = None;
                    self.file_feedback(&text);
                }
                _ => {}
            }
            return;
        }
        if let Some((key, t)) = self.summary_input.as_mut() {
            match k.code {
                KeyCode::Esc => self.summary_input = None,
                KeyCode::Backspace => {
                    t.pop();
                }
                KeyCode::Char(c) => t.push(c),
                KeyCode::Enter => {
                    let (key, text) = (std::mem::take(key), std::mem::take(t));
                    self.summary_input = None;
                    self.close_out(&key, "now", &text);
                }
                _ => {}
            }
            return;
        }
        if let Some(c) = self.confirm.take() {
            if k.code == KeyCode::Char('y') {
                let (tool, win) = match c {
                    Confirm::Reload => ("cockpit.reload", "cockpit-reload"),
                    Confirm::Update => ("cockpit.update", "cockpit-update"),
                };
                self.flash = match self.tool(tool, json!({})) {
                    Ok(_) => format!("running in the {win} window"),
                    Err(e) => e,
                };
            } else {
                self.flash = "cancelled".into();
            }
            return;
        }
        if self.projects.input.is_some() {
            return self.projects_input_key(k);
        }
        if self.tab == Tab::Settings && self.settings_state.edit.is_some() {
            return self.settings_key(k);
        }
        match k.code {
            KeyCode::Char('!') => self.fb_input = Some(String::new()),
            KeyCode::Char('R') => self.confirm = Some(Confirm::Reload),
            KeyCode::Char('U') => self.confirm = Some(Confirm::Update),
            KeyCode::Char(',') if self.tab != Tab::Settings => self.enter_settings(),
            KeyCode::Char('?') => self.show_help = !self.show_help,
            // q leaves the cockpit (it keeps running, like a daemon); Q stops the queue itself
            KeyCode::Char('q') => match &self.travel {
                Some(t) => t.detach_clients(),
                None => self.quit = true,
            },
            KeyCode::Char('Q') => self.quit = true,
            KeyCode::Tab => self.cycle(1),
            KeyCode::BackTab => self.cycle(TAB_ORDER.len() - 1),
            _ => {
                match self.tab {
                    Tab::Queue => self.queue_key(k),
                    Tab::Projects => {
                        self.projects_key(k);
                    }
                    Tab::Settings => self.settings_key(k),
                };
            }
        }
    }

    /// Tab cycles queue → projects → … → queue; the open project is kept.
    fn cycle(&mut self, by: usize) {
        let i = TAB_ORDER.iter().position(|t| *t == self.tab).unwrap_or(0);
        match TAB_ORDER[(i + by) % TAB_ORDER.len()] {
            Tab::Settings => self.enter_settings(),
            t => self.tab = t,
        }
    }

    fn mv(&mut self, d: isize) {
        let i = (self.idx() as isize + d).clamp(0, self.rows.len().saturating_sub(1) as isize) as usize;
        self.sel_id = self.rows.get(i).map(|r| r.id.clone()).unwrap_or_default();
    }

    fn queue_key(&mut self, k: KeyEvent) {
        let cur = self.cur().cloned();
        match k.code {
            KeyCode::Char('C') => match cur.filter(|r| !r.key.is_empty()) {
                Some(r) => self.picker = Some(close_out_picker(&r.key)),
                None => self.flash = "no ticket on this row".into(),
            },
            KeyCode::Char('p') => match cur.filter(|r| !r.key.is_empty()) {
                Some(r) => self.picker = self.project_picker(&r.key, false),
                None => self.flash = "no ticket on this row".into(),
            },
            // l: link meeting context; walks through project and Palaver linking first when missing
            KeyCode::Char('l') => match cur.filter(|r| !r.key.is_empty()) {
                Some(r) => {
                    self.picker = match self.tool("palaver.meetings", json!({"key": r.key, "limit": 1})) {
                        Ok(_) => self.meeting_picker(&r.key),
                        Err(e) if e.contains("has no project") => self.project_picker(&r.key, true),
                        Err(e) if e.contains("not linked") => {
                            self.palaver_picker(&r.key, r.project.as_deref().unwrap_or(""), true)
                        }
                        Err(e) => {
                            self.flash = e;
                            None
                        }
                    }
                }
                None => self.flash = "no ticket on this row".into(),
            },
            // Enter moves the cursor into the session's Claude pane; the queue stays next to it
            KeyCode::Enter => {
                let attached = self.travel.as_ref().is_some_and(|t| t.attach());
                if let (false, Some(r)) = (attached, cur) {
                    self.flash = match (r.section, r.pane, &r.cockpit_pane) {
                        (READY, ..) => "s starts a session for this ticket".into(),
                        (_, true, None) => {
                            format!("runs in your own tmux ({}): reply keys work, attach there", r.place)
                        }
                        (_, false, _) => "this session is not running in tmux: view only".into(),
                        _ => String::new(),
                    };
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.mv(1),
            KeyCode::Char('k') | KeyCode::Up => self.mv(-1),
            KeyCode::Char('r') if cur.as_ref().is_some_and(|r| r.section < ENDED) => self.input = Some(String::new()),
            KeyCode::Char(ch @ '1'..='4') => self.flash = self.send(json!({"keys": [ch.to_string()]})),
            KeyCode::Char('a') => self.flash = self.send(json!({"keys": ["Enter"]})),
            KeyCode::Char('x') => self.flash = self.send(json!({"keys": ["Escape"]})),
            KeyCode::Char(c @ ('s' | 'S')) => {
                if let Some(r) = cur.filter(|r| r.section == READY || r.section == ENDED) {
                    self.start_ticket(&r.id, c == 's');
                }
            }
            KeyCode::Char(ch) => {
                if let Some((_, text)) = self.replies().into_iter().find(|(c, _)| *c == ch) {
                    if cur.is_some_and(|r| r.section < ENDED) {
                        self.send_reply(&text);
                    }
                }
            }
            _ => {}
        }
    }

    /// `s` (`foreground`): starts the ticket's session, restarts Claude in its window when it exited, or finds it
    /// running; then the queue tab takes over and follows the ticket until its session row shows up.
    /// `S`: only starts it; tab, selection and focus stay.
    pub fn start_ticket(&mut self, key: &str, foreground: bool) {
        self.flash = match self.tool("ticket.start", json!({"key": key})) {
            Ok(r) => {
                if foreground {
                    self.tab = Tab::Queue;
                    self.pending = Some((key.to_string(), self.now));
                    self.resolve_pending();
                }
                if r["started"] == false {
                    format!("{key} is already running")
                } else if r["restarted"] == true {
                    format!("restarted {key}")
                } else {
                    format!("started {key}")
                }
            }
            Err(e) => e,
        };
    }

    /// Selects the live session row of the ticket being started, once it exists (the session id is unknown until
    /// the SessionStart hook fires). Given up after two minutes.
    fn resolve_pending(&mut self) {
        let Some((key, since)) = self.pending.clone() else { return };
        if self.now - since > 120 {
            self.pending = None;
        } else if let Some(r) = self.rows.iter().find(|r| r.key == key && r.section < ENDED) {
            self.sel_id = r.id.clone();
            self.pending = None;
        }
    }

    /// Until the session row exists, the queue travels into the window of the ticket being started.
    fn pending_focus(&self) -> Option<Row> {
        let (key, _) = self.pending.as_ref()?;
        let w = self.windows.iter().find(|w| str(w, "name") == key)?;
        Some(
            self.rows
                .iter()
                .find(|r| r.section == ENDED && &r.key == key)
                .cloned()
                .unwrap_or_else(|| queue::window_row(key, key, str(w, "pane"))),
        )
    }

    /// Calls `ticket.close_out`; a refusal (draft in the prompt, open question) is shown and nothing is marked.
    pub fn close_out(&mut self, key: &str, mode: &str, summary: &str) {
        self.flash = match self.tool("ticket.close_out", json!({"key": key, "mode": mode, "summary": summary})) {
            Ok(r) if r["ok"] == false => format!("not sent: {}", str(&r, "refused")),
            Ok(r) => str(&r, "msg").to_string(),
            Err(e) => e,
        };
    }

    fn feedback_context(&self) -> Value {
        let selected = match self.tab {
            Tab::Queue => self.cur().map(|r| format!("{} [{}] {}", r.key, queue::SECTIONS[r.section].0, r.label)),
            Tab::Projects => self.projects.cur().map(|r| r.label.clone()).or_else(|| self.projects.open.clone()),
            Tab::Settings => {
                self.settings.get(self.settings_state.sel.saturating_sub(1)).map(|s| str(s, "key").to_string())
            }
        };
        json!({"tab": format!("{:?}", self.tab), "selected": selected.unwrap_or_default(), "flash": self.flash})
    }

    /// `!`: files the text (or the last error when empty) as a ticket in the cockpit project.
    fn file_feedback(&mut self, text: &str) {
        let ctx = self.feedback_context();
        self.flash = match self.tool("cockpit.feedback", json!({"text": text, "context": ctx})) {
            Ok(r) => format!("filed {}", str(&r, "key")),
            Err(e) => e,
        };
    }

    // ---- pickers ---------------------------------------------------------------------------------------------

    pub fn project_picker(&mut self, key: &str, then_meetings: bool) -> Option<Picker> {
        let ps = self.tool("project.list", json!({})).unwrap_or(json!([]));
        let items = ps
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                let pv = p["palaver"].as_str().map(|v| format!(", Palaver: {v}")).unwrap_or_default();
                (
                    format!("◆ {}  ({} tickets{pv})", str(p, "name"), p["tickets"]),
                    str(p, "name").to_string(),
                    String::new(),
                )
            })
            .collect();
        let mut p = Picker::new(Pick::Project, format!("Project for {key} (type to create)"), items);
        p.key = key.into();
        p.then_meetings = then_meetings;
        Some(p)
    }

    fn palaver_picker(&mut self, key: &str, project: &str, then_meetings: bool) -> Option<Picker> {
        match self.tool("palaver.projects", json!({})) {
            Ok(ps) => {
                let items = ps
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        (
                            format!("{}  ({} meetings)", str(p, "name"), p["meetings"]),
                            str(p, "name").to_string(),
                            str(p, "description").to_string(),
                        )
                    })
                    .collect();
                let mut p = Picker::new(Pick::Palaver, format!("Link ◆ {project} to a Palaver project"), items);
                p.key = key.into();
                p.project = project.into();
                p.then_meetings = then_meetings;
                Some(p)
            }
            Err(e) => {
                self.flash = e;
                None
            }
        }
    }

    fn meeting_picker(&mut self, key: &str) -> Option<Picker> {
        match self.tool("palaver.meetings", json!({"key": key})) {
            Ok(ms) => {
                let items = ms
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        let mark = if m["linked"] == true { "✓" } else { " " };
                        let day = str(m, "start").get(..10).unwrap_or("");
                        (
                            format!("{mark} {day}  {}  ({} min)", str(m, "title"), m["minutes"]),
                            str(m, "id").to_string(),
                            str(m, "summary").to_string(),
                        )
                    })
                    .collect();
                let mut p = Picker::new(Pick::Meeting, format!("Meeting context for {key}"), items);
                p.key = key.into();
                Some(p)
            }
            Err(e) => {
                self.flash = e;
                None
            }
        }
    }

    fn picker_key(&mut self, k: KeyEvent) {
        let Some(p) = self.picker.as_mut() else { return };
        match picker_key(p, k) {
            PickerKey::Nothing => {}
            PickerKey::Close => self.picker = None,
            PickerKey::AutoLink => {
                let key = p.key.clone();
                self.flash = match self.tool("ticket.auto_link_meetings", json!({"key": key})) {
                    Ok(v) => format!("Claude is looking through the meetings in {}", str(&v, "window")),
                    Err(e) => e,
                };
                self.picker = None;
            }
            PickerKey::Choose => {
                let Some(v) = p.choice() else { return };
                let (kind, key, project, then) = (p.kind, p.key.clone(), p.project.clone(), p.then_meetings);
                self.picker = self.choose(kind, &key, &project, then, &v);
            }
        }
    }

    /// Acts on a picked value; returns the next picker of the meeting flow, if any.
    fn choose(&mut self, kind: Pick, key: &str, project: &str, then: bool, v: &str) -> Option<Picker> {
        match kind {
            Pick::Project => match self.tool("ticket.set_project", json!({"key": key, "project": v})) {
                Ok(_) => {
                    let linked = self
                        .tool("project.list", json!({}))
                        .ok()
                        .and_then(|ps| ps.as_array().cloned())
                        .into_iter()
                        .flatten()
                        .any(|x| str(&x, "name") == v && !x["palaver"].is_null());
                    self.flash = format!("{key} → ◆ {v}");
                    match (linked, then) {
                        (true, true) => self.meeting_picker(key),
                        (true, false) => None,
                        (false, _) => self.palaver_picker(key, v, then),
                    }
                }
                Err(e) => {
                    self.flash = e;
                    None
                }
            },
            Pick::Palaver => match self.tool("project.link_palaver", json!({"project": project, "palaver": v})) {
                Ok(_) => {
                    self.flash = format!("◆ {project} ↔ Palaver {v}");
                    if then {
                        self.meeting_picker(key)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    self.flash = e;
                    None
                }
            },
            Pick::Meeting => {
                self.flash = match self.tool("ticket.link_meeting", json!({"key": key, "meeting_id": v})) {
                    Ok(r) => format!(
                        "linked \"{}\"{}",
                        str(&r, "title"),
                        if r["pasted"] == true { ", pasted into the session's prompt" } else { "" }
                    ),
                    Err(e) => e,
                };
                None
            }
            Pick::Cluster => {
                self.flash = match self.tool("ticket.update", json!({"key": key, "cluster": v})) {
                    Ok(_) => format!("{key} → ▸ {v}"),
                    Err(e) => e,
                };
                None
            }
            Pick::CloseOut => {
                match v {
                    "now" => self.summary_input = Some((key.to_string(), String::new())),
                    mode => self.close_out(key, mode, ""),
                }
                None
            }
            Pick::OpenProject => {
                self.projects.open = Some(v.to_string());
                self.projects.psel = 0;
                self.projects.filter.clear();
                None
            }
        }
    }

    // ---- drawing ---------------------------------------------------------------------------------------------

    pub fn draw(&self, f: &mut Frame) {
        let [main, bar] = Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(f.area());
        let [left, right] = Layout::vertical([Constraint::Min(5), Constraint::Length(12)]).areas(main);
        match self.tab {
            Tab::Projects => {
                self.draw_projects(f, left, right);
                return self.draw_overlays(f, main, bar);
            }
            Tab::Settings => {
                self.draw_settings(f, main);
                return self.draw_overlays(f, main, bar);
            }
            Tab::Queue => {}
        }
        let tabs = tabs_line(0);
        let mut items = vec![ListItem::new(tabs), ListItem::new("")];
        let mut sel = None;
        if let Some(e) = &self.down {
            let what = e.split(" (").next().unwrap_or(e).to_string();
            items.push(ListItem::new(Line::from(format!(" {what}")).yellow()));
            items.push(ListItem::new(Line::from(format!("   waiting for {}", self.api.base())).dim()));
        } else {
            let (rows, s) = queue::list_items(&self.rows, self.cur().map_or("", |r| r.id.as_str()), self.now);
            sel = s.map(|s| s + 2);
            items.extend(rows);
        }
        let mut ls = ListState::default();
        ls.select(sel);
        f.render_stateful_widget(
            List::new(items)
                .block(Block::new().borders(Borders::BOTTOM))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            left,
            &mut ls,
        );
        if let Some(r) = self.cur() {
            f.render_widget(
                Paragraph::new(queue::detail(r, &self.context, self.now)).wrap(Wrap { trim: false }),
                right,
            );
        }
        self.draw_overlays(f, main, bar);
    }

    fn draw_overlays(&self, f: &mut Frame, main: Rect, bar: Rect) {
        if let Some(p) = &self.picker {
            render_picker(f, main, p);
        }
        f.render_widget(Paragraph::new(self.footer()).wrap(Wrap { trim: true }), bar);
    }

    fn footer(&self) -> Line<'static> {
        if let Some(t) = &self.input {
            return input_line("reply", t);
        }
        if let Some(t) = &self.fb_input {
            return input_line("improve the cockpit", t);
        }
        if let Some((k, t)) = &self.summary_input {
            return input_line(&format!("summary for {k} (may be empty)"), t);
        }
        if let Some(c) = self.confirm {
            let what = match c {
                Confirm::Reload => {
                    "Reload the cockpit (restart the daemon, replace the queue with the installed binary)?".to_string()
                }
                Confirm::Update => {
                    format!("Update the cockpit ({})? It builds/downloads in a window, then reloads.", self.channel())
                }
            };
            return Line::from(format!(" {what} (y/n)")).yellow();
        }
        if let Some(l) = self.projects_input_line() {
            return l;
        }
        if !self.flash.is_empty() {
            return Line::from(format!(" {}", self.flash)).yellow();
        }
        if self.show_help {
            return Line::from(match self.tab {
                Tab::Queue => " ⇥ tabs · j/k move · ⏎ attach (⇧esc back) · , settings · s start · S start in background · p project · l link meeting · C close out · r reply (quick replies) · 1-4 pick · a accept · x interrupt · ! improve the cockpit · R reload · U update · q leave · Q stop queue",
                Tab::Projects => self.projects_help(),
                Tab::Settings => " ⇥ queue · j/k move · ⏎ change · r default · R reload · U update · ! improve the cockpit · q leave",
            })
            .dim();
        }
        if self.stale_daemon {
            return Line::from(" daemon is a different build: cockpit daemon restart").yellow();
        }
        if !self.update["available"].is_null() {
            let what = match self.update["commits_ahead"].as_i64() {
                Some(1) => "1 commit".to_string(),
                Some(n) => format!("{n} commits"),
                None => str(&self.update, "available").to_string(),
            };
            return Line::from(format!(" update available: {what} · U to update")).green();
        }
        Line::from(" ⇥ tabs · ? all keys · q leave").dim()
    }

    /// The update channel as the setting has it (the daemon's check says it too, once it has run).
    fn channel(&self) -> String {
        match self.setting("update_channel").as_str().or(self.update["channel"].as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => "local".into(),
        }
    }
}

/// Close-out menu: Enter takes the first entry.
pub fn close_out_picker(key: &str) -> Picker {
    let items = vec![
        ("Wrap up and close".to_string(), "wrap".to_string(), "Asks the session to finish what the ticket still needs, commit if appropriate, and close it with a summary. The window closes when it is done.".to_string()),
        ("Close now".to_string(), "now".to_string(), "Marks the ticket done with a summary you type, ends the session and closes the window.".to_string()),
        ("Close window, keep ticket".to_string(), "keep".to_string(), "Ticket goes back to todo, session ended, window closed.".to_string()),
        ("Drop ticket".to_string(), "drop".to_string(), "Ticket canceled, window closed.".to_string()),
    ];
    let mut p = Picker::new(Pick::CloseOut, format!("Close out {key}"), items);
    p.key = key.into();
    p
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::util::ms_from_iso;
    use ratatui::{backend::TestBackend, Terminal};

    pub fn app() -> App {
        let mut a = App::new(Api::new("http://127.0.0.1:1"), Tmux::private("app-unit"), None, "%0");
        a.now = ms_from_iso("2026-11-18T09:42:00Z").unwrap() / 1000;
        a.load_state(&queue::sample_state());
        a.settings = vec![json!({"key": "quick_replies", "value": "c=continue; y=yes"})];
        a
    }

    pub fn screen(a: &App, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect()
    }

    pub fn press(a: &mut App, c: KeyCode) {
        a.handle_key(KeyEvent::new(c, ratatui::crossterm::event::KeyModifiers::NONE));
    }

    #[test]
    fn queue_shows_sections_rows_and_chips_under_the_selection() {
        let a = app();
        let s = screen(&a, 70, 30);
        assert_eq!(
            &s[..12],
            [
                " queue   projects   settings   ⇥",
                "",
                "NEEDS YOU 1",
                " DEM-1  Login 42m",
                "   demo · feat/x · cockpit:DEM-1 · ◆ Demo",
                "   [⏎] attach  [1-4] pick  [a] accept  [r] reply  [x] interrupt  [C] c",
                "",
                "YOUR TURN 1",
                " demo · main 32m",
                "   demo · main · main:zsh",
                "",
                "WORKING 1",
            ]
        );
        // the detail block shows the message under the label
        assert!(s.iter().any(|l| l.contains("NEEDS YOU 42m · demo · feat/x")));
        assert!(s.iter().any(|l| l == "which db?"));
        assert_eq!(s.last().map(String::as_str), Some(""));
        assert!(s.iter().any(|l| l == "⇥ tabs · ? all keys · q leave"));
    }

    #[test]
    fn ended_section_follows_working_and_shows_its_chips() {
        let mut a = app();
        a.load_state(&queue::ended_state());
        a.sel_id = "DEM-3".into();
        let s = screen(&a, 70, 40);
        let at = |t: &str| s.iter().position(|l| l.starts_with(t)).unwrap_or_else(|| panic!("{t} in {s:#?}"));
        assert!(at("WORKING 1") < at("ENDED 1") && at("ENDED 1") < at("READY 1"));
        assert!(s.iter().any(|l| l == "   [s] restart session  [⏎] open window  [C] close out"), "{s:#?}");
        // replies are refused, with the way out
        press(&mut a, KeyCode::Char('1'));
        assert!(a.flash.contains("s restarts it"), "{}", a.flash);
    }

    #[test]
    fn a_daemon_of_another_build_shows_a_notice_but_keeps_the_queue() {
        let mut a = app();
        let s = screen(&a, 70, 30);
        assert!(!s.iter().any(|l| l.contains("different build")));
        a.stale_daemon = true;
        let s = screen(&a, 70, 30);
        assert!(s.iter().any(|l| l == "daemon is a different build: cockpit daemon restart"), "{s:#?}");
        assert!(s.iter().any(|l| l.starts_with("NEEDS YOU")), "the queue is still drawn");
        // a flash (a failed tool) still wins the line
        a.flash = "no ticket DEM-9".into();
        assert!(screen(&a, 70, 30).iter().any(|l| l == "no ticket DEM-9"));
    }

    fn started_json() -> Vec<(&'static str, &'static str)> {
        vec![("200 OK", r#"{"window":"cockpit:DEM-2","started":true}"#)]
    }

    #[test]
    fn s_goes_to_the_queue_and_follows_the_ticket_until_its_session_appears() {
        let mut a = app();
        a.api = super::super::api::tests::canned(started_json());
        a.tab = Tab::Projects;
        a.projects.open = Some("Demo".into());
        a.projects.set_overview(projects::sample_overview());
        a.projects.psel = a.projects.rows.iter().position(|r| r.key == "DEM-2").unwrap();
        press(&mut a, KeyCode::Char('s'));
        assert_eq!((a.tab, a.flash.as_str()), (Tab::Queue, "started DEM-2"));
        // no session yet: the queue travels into the window, and a refresh does not move the selection away
        let mut st = queue::sample_state();
        st["windows"] = json!([{"name": "DEM-2", "pane": "cockpit:%20", "claude": true}]);
        a.load_state(&st);
        assert_eq!(a.focus().and_then(|r| r.cockpit_pane), Some("%20".into()));
        // the SessionStart hook fired: the session claims the ticket and its row is selected
        st["tickets"][1]["status"] = json!("doing");
        st["tickets"][1]["session"] = json!("s9");
        st["sessions"].as_array_mut().unwrap().push(json!({"id": "s9", "state": "turn", "state_at": "2026-11-18T09:40:00Z", "repo": "demo", "tmux_pane": "cockpit:%20", "git": {}}));
        st["where"]["cockpit:%20"] = json!("cockpit:DEM-2");
        st["ready"] = json!([]);
        a.load_state(&st);
        assert_eq!(a.sel_id, "s9");
        assert!(a.pending.is_none());
        // a key press before that takes over: nothing is selected behind the user's back
        a.pending = Some(("DEM-9".into(), a.now));
        press(&mut a, KeyCode::Char('j'));
        assert!(a.pending.is_none());
    }

    #[test]
    fn shift_s_starts_without_moving_anything() {
        let mut a = app();
        a.api = super::super::api::tests::canned(started_json());
        a.sel_id = "DEM-2".into();
        press(&mut a, KeyCode::Char('S'));
        assert_eq!((a.tab, a.sel_id.as_str(), a.flash.as_str()), (Tab::Queue, "DEM-2", "started DEM-2"));
        assert!(a.pending.is_none());
        a.api = super::super::api::tests::canned(started_json());
        a.tab = Tab::Projects;
        a.projects.open = Some("Demo".into());
        a.projects.set_overview(projects::sample_overview());
        a.projects.psel = a.projects.rows.iter().position(|r| r.key == "DEM-3").unwrap();
        press(&mut a, KeyCode::Char('S'));
        assert_eq!((a.tab, a.flash.as_str()), (Tab::Projects, "started DEM-3"));
        assert!(a.pending.is_none());
    }

    #[test]
    fn already_running_selects_the_live_row_at_once() {
        let mut a = app();
        a.api = super::super::api::tests::canned(vec![(
            "200 OK",
            r#"{"window":"cockpit:DEM-1","started":false,"reason":"already running"}"#,
        )]);
        a.tab = Tab::Projects;
        a.projects.open = Some("Demo".into());
        a.projects.set_overview(projects::sample_overview());
        a.projects.psel = a.projects.rows.iter().position(|r| r.key == "DEM-1").unwrap();
        press(&mut a, KeyCode::Char('s'));
        assert_eq!((a.tab, a.sel_id.as_str(), a.flash.as_str()), (Tab::Queue, "s1", "DEM-1 is already running"));
    }

    #[test]
    fn moving_the_selection_moves_the_chips_and_the_detail() {
        let mut a = app();
        for _ in 0..3 {
            press(&mut a, KeyCode::Char('j'));
        }
        assert_eq!(a.cur().map(|r| r.id.as_str()), Some("DEM-2"));
        let s = screen(&a, 70, 40);
        assert!(
            s.iter().any(|l| l.starts_with("   [s] start  [S] start in background  [p] project  [l] meeting")),
            "{s:#?}"
        );
        assert!(s.iter().any(|l| l == "do it"));
        // going past the end stays on the last row; k goes back
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(a.sel_id, "DEM-2");
        press(&mut a, KeyCode::Char('k'));
        assert_eq!(a.sel_id, "s2");
    }

    #[test]
    fn reply_line_and_help_and_flash() {
        let mut a = app();
        press(&mut a, KeyCode::Char('r'));
        for c in "hi".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(screen(&a, 70, 30)[26], "reply › hi▏");
        press(&mut a, KeyCode::Esc);
        assert!(a.input.is_none());
        press(&mut a, KeyCode::Char('?'));
        assert!(screen(&a, 70, 30)[26].contains("⏎ attach (⇧esc back)"));
        press(&mut a, KeyCode::Char('?'));
        // a refused or failed action is the footer; the daemon is down in tests, so the refusal is its message
        press(&mut a, KeyCode::Char('a'));
        assert!(a.flash.contains("daemon not running"), "{}", a.flash);
        assert!(screen(&a, 70, 30)[26].starts_with("daemon not running"));
        // ready rows take no reply
        a.sel_id = "DEM-2".into();
        press(&mut a, KeyCode::Char('r'));
        assert!(a.input.is_none());
    }

    #[test]
    fn a_missing_daemon_is_a_message_not_a_crash() {
        let mut a = app();
        a.tick();
        assert!(a.down.is_some());
        let s = screen(&a, 70, 20);
        assert_eq!(s[2], " daemon not running: cockpit daemon start");
        assert_eq!(s[3], "   waiting for http://127.0.0.1:1");
    }

    fn with_projects() -> App {
        let mut a = app();
        a.projects.list = vec![
            json!({"name": "Demo", "planned": 2, "doing": 1, "done": 1, "palaver": "Demo Palaver"}),
            json!({"name": "Docs", "planned": 0, "doing": 0, "done": 0, "palaver": null}),
        ];
        a
    }

    #[test]
    fn projects_list_then_view_and_back() {
        let mut a = with_projects();
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Projects);
        let s = screen(&a, 80, 30);
        assert_eq!(
            &s[..7],
            [
                " queue   projects   settings   ⇥",
                "PROJECTS 2",
                "",
                " ◆ Demo",
                "     ○ 2 planned · ◐ 1 in progress · ✓ 1 done · Palaver: Demo Palaver",
                " ◆ Docs",
                "     ○ 0 planned · ◐ 0 in progress · ✓ 0 done",
            ]
        );
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.projects.open.as_deref(), Some("Demo"));
        a.projects.set_overview(projects::sample_overview());
        assert_eq!(a.projects.psel, 2, "the cursor skips headers");
        let s = screen(&a, 80, 30);
        assert_eq!(
            &s[..12],
            [
                " queue   projects   settings   ⇥",
                "◆ Demo",
                "",
                "PLANNED 2",
                "  ▸ auth",
                " ○ DEM-2  Ready one",
                "     P1 · ▸ auth",
                "   [s] start  [S] start in background  [g] cluster  [C] close out",
                "  ▸ unsorted",
                " ○ DEM-3  Polish",
                "     P3",
                "",
            ]
        );
        assert!(s.iter().any(|l| l == "do it"), "the body of the selected ticket");
        press(&mut a, KeyCode::Char('j'));
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(a.projects.cur().map(|r| r.key.as_str()), Some("DEM-1"));
        assert!(screen(&a, 80, 40).iter().any(|l| l == "   [⏎] open session  [g] cluster  [C] close out"));
        // ⏎ on a ticket with a live session opens it in the queue
        press(&mut a, KeyCode::Enter);
        assert_eq!((a.tab, a.sel_id.as_str()), (Tab::Queue, "s1"));
    }

    #[test]
    fn tab_cycling_keeps_the_open_project() {
        let mut a = with_projects();
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Enter);
        a.projects.set_overview(projects::sample_overview());
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Settings);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Queue);
        press(&mut a, KeyCode::BackTab);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.tab, Tab::Projects);
        assert_eq!(a.projects.open.as_deref(), Some("Demo"));
        // esc goes back to the list; o opens the fuzzy picker
        press(&mut a, KeyCode::Char('o'));
        assert!(a.picker.is_some());
        for c in "doc".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.projects.open.as_deref(), Some("Docs"));
        press(&mut a, KeyCode::Esc);
        assert!(a.projects.open.is_none());
    }

    #[test]
    fn filter_narrows_locally_while_typing() {
        let mut a = with_projects();
        a.tab = Tab::Projects;
        a.projects.open = Some("Demo".into());
        a.projects.set_overview(projects::sample_overview());
        press(&mut a, KeyCode::Char('/'));
        for c in "pol".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        let s = screen(&a, 80, 30);
        assert_eq!(s[1], "◆ Demo   / pol");
        assert!(s.iter().any(|l| l.contains("DEM-3  Polish")) && !s.iter().any(|l| l.contains("DEM-2")));
        assert!(s.iter().any(|l| l == "filter › pol▏"));
        press(&mut a, KeyCode::Esc);
        assert!(a.projects.filter.is_empty());
        assert!(screen(&a, 80, 30).iter().any(|l| l.contains("DEM-2")));
    }

    #[test]
    fn cluster_picker_offers_a_new_cluster_last() {
        let mut a = with_projects();
        a.tab = Tab::Projects;
        a.projects.open = Some("Demo".into());
        a.projects.set_overview(projects::sample_overview());
        press(&mut a, KeyCode::Char('g'));
        for c in "a".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        let shown: Vec<String> = a.picker.as_ref().unwrap().shown().into_iter().map(|x| x.0).collect();
        assert_eq!(shown, ["▸ auth", "+ new cluster \"a\""]);
        let s = screen(&a, 80, 30);
        assert!(s.iter().any(|l| l.contains("Cluster for DEM-2 (type to create)")));
    }

    fn with_settings() -> App {
        let mut a = app();
        a.settings = settings::sample_settings();
        a.settings_state.conns = settings::sample_conns();
        a.tab = Tab::Settings;
        a
    }

    #[test]
    fn settings_tab_lists_settings_and_connections() {
        let mut a = with_settings();
        a.settings_state.sel = 1;
        let s = screen(&a, 80, 30);
        assert_eq!(
            &s[..20],
            [
                " queue   projects   settings   ⇥",
                "",
                " SETTINGS",
                " Update   ·",
                "   not checked yet",
                " queue key  S-Escape",
                "   tmux key that jumps back to the queue",
                "   [⏎] edit  [r] default",
                " confirm interrupt  [x]",
                "   Ask before Esc interrupts",
                " queue width  40",
                "   Width of the queue pane in percent",
                "",
                " CONNECTIONS",
                " ● Claude Code hooks  ok",
                "     7/7 events",
                " ○ Jira  planned",
                "     placeholder",
                "",
                "",
            ]
        );
        assert_eq!(a.settings_slot_key(), "settings:1:\"S-Escape\"");
    }

    #[test]
    fn settings_selection_walks_into_the_connections() {
        let mut a = with_settings();
        for _ in 0..4 {
            press(&mut a, KeyCode::Char('j'));
        }
        assert_eq!(a.settings_state.sel, 4);
        assert!(a.settings_slot_text().contains("Claude Code hooks"));
        assert_eq!(a.settings_slot_key(), "settings:4:ok");
        for _ in 0..9 {
            press(&mut a, KeyCode::Char('j'));
        }
        assert_eq!(a.settings_state.sel, 5, "stops at the last connection");
        // toggling a checkbox with the daemon away shows the refusal; the value is untouched
        a.settings_state.sel = 2;
        press(&mut a, KeyCode::Enter);
        assert!(a.flash.contains("daemon not running"));
        // a number is edited inline
        a.settings_state.sel = 3;
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Backspace);
        press(&mut a, KeyCode::Char('5'));
        assert!(screen(&a, 80, 30).iter().any(|l| l == " queue width  45▏"));
        press(&mut a, KeyCode::Esc);
        assert!(a.settings_state.edit.is_none() && a.tab == Tab::Settings);
    }

    #[test]
    fn tabs_cycle_forward_and_back_and_comma_jumps_to_settings() {
        let mut a = app();
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Settings);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Queue);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.tab, Tab::Settings);
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.tab, Tab::Queue);
        press(&mut a, KeyCode::Char(','));
        assert_eq!(a.tab, Tab::Settings);
        press(&mut a, KeyCode::Char(','));
        assert_eq!(a.tab, Tab::Queue);
    }

    /// A daemon that records each request (path, JSON body) and answers with the next canned body.
    fn recording(replies: Vec<&'static str>) -> (Api, std::sync::mpsc::Receiver<(String, Value)>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (body, s) in replies.into_iter().zip(l.incoming().flatten()) {
                let mut r = BufReader::new(s);
                let mut first = String::new();
                r.read_line(&mut first).unwrap();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).unwrap();
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let _ = tx.send((path, serde_json::from_slice(&buf).unwrap_or(Value::Null)));
                let _ = write!(
                    r.get_mut(),
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (Api::new(&url), rx)
    }

    fn type_text(a: &mut App, t: &str) {
        for c in t.chars() {
            press(a, KeyCode::Char(c));
        }
    }

    #[test]
    fn close_out_picker_lists_wrap_up_first_and_enter_takes_it() {
        let (api, rx) = recording(vec![
            r#"{"ok":true,"mode":"wrap","msg":"asked DEM-1's session to wrap up; the window closes when it is done"}"#,
        ]);
        let mut a = app();
        a.api = api;
        press(&mut a, KeyCode::Char('C'));
        let p = a.picker.as_ref().unwrap();
        assert_eq!(p.title, "Close out DEM-1");
        let labels: Vec<_> = p.items.iter().map(|i| (i.0.as_str(), i.1.as_str())).collect();
        assert_eq!(
            labels,
            [
                ("Wrap up and close", "wrap"),
                ("Close now", "now"),
                ("Close window, keep ticket", "keep"),
                ("Drop ticket", "drop")
            ]
        );
        assert!(screen(&a, 90, 30).iter().any(|l| l.contains("Wrap up and close")));
        press(&mut a, KeyCode::Enter);
        assert!(a.picker.is_none());
        assert_eq!(a.flash, "asked DEM-1's session to wrap up; the window closes when it is done");
        let (path, body) = rx.recv().unwrap();
        assert_eq!(path, "/api/tools/ticket.close_out");
        assert_eq!((body["key"].as_str(), body["mode"].as_str()), (Some("DEM-1"), Some("wrap")));
    }

    #[test]
    fn a_refused_wrap_up_is_shown_not_marked() {
        let (api, _rx) = recording(vec![
            r#"{"ok":false,"refused":"the prompt already has text in it (\"fix\"): attach to send or clear it"}"#,
        ]);
        let mut a = app();
        a.api = api;
        press(&mut a, KeyCode::Char('C'));
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.flash, "not sent: the prompt already has text in it (\"fix\"): attach to send or clear it");
    }

    #[test]
    fn close_now_asks_for_a_summary_the_others_act_at_once() {
        let (api, rx) = recording(vec![
            r#"{"ok":true,"msg":"closed DEM-1"}"#,
            r#"{"ok":true,"msg":"DEM-1 back to todo, window closed"}"#,
            r#"{"ok":true,"msg":"DEM-1 dropped"}"#,
        ]);
        let mut a = app();
        a.api = api;
        press(&mut a, KeyCode::Char('C'));
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.summary_input, Some(("DEM-1".into(), String::new())));
        assert!(screen(&a, 70, 20).iter().any(|l| l.starts_with("summary for DEM-1 (may be empty) ›")));
        type_text(&mut a, "all good");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.flash, "closed DEM-1");
        let (_, body) = rx.recv().unwrap();
        assert_eq!((body["mode"].as_str(), body["summary"].as_str()), (Some("now"), Some("all good")));
        // esc cancels without calling anything
        press(&mut a, KeyCode::Char('C'));
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Esc);
        assert!(a.summary_input.is_none());
        for (downs, mode) in [(2, "keep"), (3, "drop")] {
            press(&mut a, KeyCode::Char('C'));
            for _ in 0..downs {
                press(&mut a, KeyCode::Down);
            }
            press(&mut a, KeyCode::Enter);
            assert_eq!(rx.recv().unwrap().1["mode"], mode);
        }
        assert_eq!(a.flash, "DEM-1 dropped");
    }

    #[test]
    fn exclamation_files_feedback_with_context_and_empty_files_the_last_error() {
        let (api, rx) = recording(vec![
            r#"{"key":"TIC-9","project":"ticket-cockpit","title":"x"}"#,
            r#"{"key":"TIC-10","project":"ticket-cockpit","title":"Fix: y"}"#,
        ]);
        let mut a = app();
        a.api = api;
        press(&mut a, KeyCode::Char('!'));
        assert!(screen(&a, 70, 20).iter().any(|l| l.starts_with("improve the cockpit ›")));
        type_text(&mut a, "queue jumps");
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.flash, "filed TIC-9");
        let (path, body) = rx.recv().unwrap();
        assert_eq!(path, "/api/tools/cockpit.feedback");
        assert_eq!(body["text"], "queue jumps");
        assert_eq!(body["context"]["tab"], "Queue");
        assert!(body["context"]["selected"].as_str().unwrap().starts_with("DEM-1"), "{body}");
        press(&mut a, KeyCode::Char('!'));
        press(&mut a, KeyCode::Enter);
        assert_eq!(rx.recv().unwrap().1["text"], "");
        assert_eq!(a.flash, "filed TIC-10");
        // while typing a reply, ! is just a character
        press(&mut a, KeyCode::Char('r'));
        press(&mut a, KeyCode::Char('!'));
        assert!(a.fb_input.is_none() && a.input.as_deref() == Some("!"));
    }

    #[test]
    fn reload_and_update_ask_first() {
        let (api, rx) = recording(vec![r#"{"window":"cockpit:cockpit-update"}"#]);
        let mut a = app();
        a.api = api;
        press(&mut a, KeyCode::Char('R'));
        let f = screen(&a, 120, 20);
        assert!(
            f.iter().any(|l| l.trim()
                == "Reload the cockpit (restart the daemon, replace the queue with the installed binary)? (y/n)"),
            "{f:#?}"
        );
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.flash, "cancelled");
        a.flash.clear();
        a.settings.push(json!({"key": "update_channel", "value": "release"}));
        press(&mut a, KeyCode::Char('U'));
        let f = screen(&a, 120, 20);
        assert!(
            f.iter()
                .any(|l| l.trim()
                    == "Update the cockpit (release)? It builds/downloads in a window, then reloads. (y/n)"),
            "{f:#?}"
        );
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(a.flash, "running in the cockpit-update window");
        assert_eq!(rx.recv().unwrap().0, "/api/tools/cockpit.update");
    }

    #[test]
    fn the_footer_recommends_an_update_but_a_flash_wins() {
        let mut a = app();
        assert!(screen(&a, 90, 30).iter().any(|l| l == "⇥ tabs · ? all keys · q leave"));
        a.update = json!({"channel": "local", "available": "abc123", "commits_ahead": 3});
        assert!(screen(&a, 90, 30).iter().any(|l| l == "update available: 3 commits · U to update"));
        a.update = json!({"channel": "local", "available": "abc123", "commits_ahead": 1});
        assert!(screen(&a, 90, 30).iter().any(|l| l == "update available: 1 commit · U to update"));
        a.update = json!({"channel": "release", "available": "v0.3.0", "commits_ahead": null});
        assert!(screen(&a, 90, 30).iter().any(|l| l == "update available: v0.3.0 · U to update"));
        a.flash = "no ticket on this row".into();
        let f = screen(&a, 90, 30);
        assert!(
            f.iter().any(|l| l == "no ticket on this row") && !f.iter().any(|l| l.contains("update available")),
            "{f:#?}"
        );
    }

    #[test]
    fn the_settings_tab_opens_on_the_update_entry() {
        let mut a = with_settings();
        a.update = json!({"channel": "local", "installed": "0.1.0 (dc34153)", "available": "abc123", "commits_ahead": 2,
            "behind_remote": 0, "summary": "update available: 2 commits", "notes": "abc123 second\ndef456 first"});
        let s = screen(&a, 100, 30);
        assert!(s.iter().any(|l| l.starts_with(" Update  local · 0.1.0 (dc34153)")), "{s:#?}");
        assert!(s.iter().any(|l| l.contains("update available: 2 commits")));
        assert!(s.iter().any(|l| l.starts_with("   [U] update  [R] reload")), "chips under the selected entry: {s:#?}");
        assert!(a.settings_slot_text().contains("abc123 second") && a.settings_slot_text().contains("Commits"));
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.confirm, Some(Confirm::Update));
        // release channel lists the notes under their own heading
        a.update = json!({"channel": "release", "installed": "0.2.0 (aaa)", "available": "v0.3.0", "summary": "update available: v0.3.0", "notes": "- fixed it"});
        assert!(a.settings_slot_text().contains("Release notes") && a.settings_slot_text().contains("- fixed it"));
    }
}
