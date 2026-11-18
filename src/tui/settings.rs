//! The settings tab: the cockpit's settings (checkbox or value) and the connections list (status only).
//! The right side, in the home window, explains the selected entry.
use super::app::{App, Tab};
use super::widgets::{chips, str, tabs_line};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent},
    layout::Rect,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{List, ListItem, ListState},
    Frame,
};
use serde_json::{json, Value};

#[derive(Default)]
pub struct State {
    pub sel: usize,
    /// The value being typed for the selected setting.
    pub edit: Option<String>,
    pub conns: Vec<Value>,
}

/// The explanation for entry `i`: a setting, or a connection after the settings. ANSI-styled; the queue
/// width gets a drawing of the split at its current value.
pub fn explain(settings: &[Value], conns: &[Value], i: usize) -> String {
    let b = |t: &str| format!("\x1b[1m{t}\x1b[0m");
    let d = |t: &str| format!("\x1b[2m{t}\x1b[0m");
    let indent =
        |t: &str| t.lines().map(|l| if l.is_empty() { "\n".into() } else { format!("  {l}\n") }).collect::<String>();
    if let Some(st) = settings.get(i) {
        let mut o = format!(
            "\n  {}\n  {}\n\n  value    {}\n  default  {}\n\n{}",
            b(&str(st, "key").replace('_', " ")),
            d(str(st, "desc")),
            st["value"],
            st["default"],
            indent(str(st, "help"))
        );
        if str(st, "key") == "queue_width" {
            let w = st["value"].as_i64().unwrap_or(38).clamp(10, 90) as usize;
            let total = 60;
            let q = total * w / 100;
            let r = total - q;
            o += &format!(
                "\n  {}\n  ┌{}┬{}┐\n  │{:^q$}│{:^r$}│\n  │{:^q$}│{:^r$}│\n  └{}┴{}┘\n",
                d(&format!("at {w}%:")),
                "─".repeat(q),
                "─".repeat(r),
                "queue",
                "Claude",
                format!("{w}%"),
                format!("{}%", 100 - w),
                "─".repeat(q),
                "─".repeat(r),
            );
        }
        return o;
    }
    match conns.get(i.wrapping_sub(settings.len())) {
        Some(c) => format!(
            "\n  {}   {}\n  {}\n\n{}",
            b(str(c, "name")),
            str(c, "status"),
            d(str(c, "detail")),
            indent(str(c, "help"))
        ),
        None => String::new(),
    }
}

/// `(glyph, color)` of a connection status.
pub fn status_mark(status: &str) -> (&'static str, Color) {
    match status {
        "ok" => ("●", Color::Green),
        "partial" => ("◐", Color::Yellow),
        "planned" => ("○", Color::Reset),
        _ => ("✕", Color::Red),
    }
}

impl App {
    pub fn enter_settings(&mut self) {
        self.tab = Tab::Settings;
        self.settings_state.conns =
            self.api.tool("connections.status", json!({})).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    }

    /// Identifies the explanation shown, so the right pane is redrawn only when it changes.
    pub(super) fn settings_slot_key(&self) -> String {
        let s = &self.settings_state;
        match self.settings.get(s.sel) {
            Some(st) => format!("settings:{}:{}", s.sel, st["value"]),
            None => format!(
                "settings:{}:{}",
                s.sel,
                s.conns.get(s.sel.wrapping_sub(self.settings.len())).map(|c| str(c, "status")).unwrap_or("")
            ),
        }
    }

    pub(super) fn settings_slot_text(&self) -> String {
        explain(&self.settings, &self.settings_state.conns, self.settings_state.sel)
    }

    fn set_setting(&mut self, key: &str, value: Value) {
        self.flash = self.api.tool("settings.set", json!({"key": key, "value": value})).err().unwrap_or_default();
    }

    /// Mirrors the confirmation setting into tmux right away, so the tick does not read it as "never ask again".
    fn mirror_confirm(&self, on: bool) {
        let _ =
            self.tmux.run(crate::tmux::Server::Cockpit, &["set", "-g", "@tk_confirm_esc", if on { "1" } else { "0" }]);
    }

    pub(super) fn settings_key(&mut self, k: KeyEvent) {
        let cur = self.settings.get(self.settings_state.sel).cloned();
        if let Some(t) = self.settings_state.edit.as_mut() {
            match k.code {
                KeyCode::Esc => self.settings_state.edit = None,
                KeyCode::Backspace => {
                    t.pop();
                }
                KeyCode::Char(c) => t.push(c),
                KeyCode::Enter => {
                    let text = std::mem::take(t);
                    self.settings_state.edit = None;
                    if let Some(st) = &cur {
                        if st["value"].is_number() {
                            match text.trim().parse::<i64>() {
                                Ok(n) => self.set_setting(str(st, "key"), n.into()),
                                Err(_) => self.flash = format!("{} expects a whole number", str(st, "key")),
                            }
                        } else {
                            self.set_setting(str(st, "key"), Value::String(text));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        let last = (self.settings.len() + self.settings_state.conns.len()).saturating_sub(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char(',') => self.tab = Tab::Queue,
            KeyCode::Char('j') | KeyCode::Down => self.settings_state.sel = (self.settings_state.sel + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.settings_state.sel = self.settings_state.sel.saturating_sub(1),
            KeyCode::Char('r') => {
                if let Some(st) = &cur {
                    self.set_setting(str(st, "key"), st["default"].clone());
                    if str(st, "key") == "confirm_interrupt" {
                        self.mirror_confirm(true);
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(st) = &cur {
                    match &st["value"] {
                        Value::Bool(b) => {
                            self.set_setting(str(st, "key"), Value::Bool(!b));
                            if str(st, "key") == "confirm_interrupt" {
                                self.mirror_confirm(!b);
                            }
                        }
                        Value::String(t) => self.settings_state.edit = Some(t.clone()),
                        v => self.settings_state.edit = Some(v.to_string()),
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let s = &self.settings_state;
        let mut items: Vec<ListItem> = vec![
            ListItem::new(tabs_line(self.tab as usize)),
            ListItem::new(""),
            ListItem::new(Line::from(" SETTINGS").bold()),
        ];
        for (i, st) in self.settings.iter().enumerate() {
            let val = match (&s.edit, i == s.sel) {
                (Some(t), true) => format!("{t}▏"),
                _ => match &st["value"] {
                    Value::Bool(b) => (if *b { "[x]" } else { "[ ]" }).to_string(),
                    Value::String(t) => t.clone(),
                    v => v.to_string(),
                },
            };
            let mut lines = vec![
                Line::from(vec![Span::raw(format!(" {}  ", str(st, "key").replace('_', " "))).bold(), Span::raw(val)]),
                Line::from(format!("   {}", str(st, "desc"))).dim(),
            ];
            if i == s.sel && s.edit.is_none() {
                lines.push(chips(&[
                    ("⏎", if st["value"].is_boolean() { "toggle".into() } else { "edit".into() }),
                    ("r", "default".into()),
                ]));
            }
            items.push(ListItem::new(lines));
        }
        items.push(ListItem::new(""));
        items.push(ListItem::new(Line::from(" CONNECTIONS").bold()));
        for c in &s.conns {
            let (dot, color) = status_mark(str(c, "status"));
            items.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!(" {dot} "), Style::new().fg(color)),
                    Span::raw(str(c, "name").to_string()).bold(),
                    Span::raw(format!("  {}", str(c, "status"))).dim(),
                ]),
                Line::from(format!("     {}", str(c, "detail"))).dim(),
            ]));
        }
        let mut ls = ListState::default();
        ls.select(Some(if s.sel < self.settings.len() { s.sel + 3 } else { s.sel + 5 }));
        f.render_stateful_widget(
            List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            area,
            &mut ls,
        );
    }
}

#[cfg(test)]
pub fn sample_settings() -> Vec<Value> {
    vec![
        json!({"key": "queue_key", "value": "S-Escape", "default": "S-Escape", "desc": "tmux key that jumps back to the queue", "help": "Pressed anywhere.\n\nNames follow tmux."}),
        json!({"key": "confirm_interrupt", "value": true, "default": true, "desc": "Ask before Esc interrupts", "help": "h"}),
        json!({"key": "queue_width", "value": 40, "default": 38, "desc": "Width of the queue pane in percent", "help": "How much."}),
    ]
}

#[cfg(test)]
pub fn sample_conns() -> Vec<Value> {
    vec![
        json!({"name": "Claude Code hooks", "status": "ok", "detail": "7/7 events", "help": "How it knows."}),
        json!({"name": "Jira", "status": "planned", "detail": "placeholder", "help": "Later."}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explanation_of_a_setting_has_value_default_and_help() {
        let e = explain(&sample_settings(), &[], 0);
        assert!(e.contains("\x1b[1mqueue key\x1b[0m"));
        assert!(e.contains("value    \"S-Escape\"") && e.contains("default  \"S-Escape\""));
        assert!(e.contains("  Pressed anywhere.\n\n  Names follow tmux.\n"));
        assert!(!e.contains("at 40%"));
    }

    #[test]
    fn queue_width_draws_the_split() {
        let e = explain(&sample_settings(), &[], 2);
        assert!(e.contains("at 40%:"));
        // 60 columns: 24 for the queue, 36 for Claude
        let top = e.lines().find(|l| l.contains('┬')).unwrap();
        assert_eq!(top.trim(), format!("┌{}┬{}┐", "─".repeat(24), "─".repeat(36)));
        assert!(e.contains("40%") && e.contains("60%"));
    }

    #[test]
    fn explanation_of_a_connection_follows_the_settings() {
        let e = explain(&sample_settings(), &sample_conns(), 4);
        assert!(e.contains("\x1b[1mJira\x1b[0m   planned"));
        assert!(e.contains("placeholder") && e.contains("  Later."));
        assert_eq!(explain(&sample_settings(), &sample_conns(), 9), "");
    }

    #[test]
    fn status_marks() {
        assert_eq!(status_mark("ok").0, "●");
        assert_eq!(status_mark("off").0, "✕");
        assert_eq!(status_mark("planned").0, "○");
    }
}
