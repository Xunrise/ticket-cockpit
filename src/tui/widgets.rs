//! Shared bits: hint chips, the fuzzy picker, small text helpers.
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;

pub fn str<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

/// Subsequence match, case-insensitive.
pub fn fuzzy(q: &str, s: &str) -> bool {
    let mut it = s.chars().flat_map(char::to_lowercase);
    q.chars().flat_map(char::to_lowercase).all(|c| it.any(|x| x == c))
}

/// Waiting time as `42s`, `7m`, `1h05m`, `3d`.
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

/// Hotkey hints shown where you look: under the selected row, bold blue keys.
pub fn chips(h: &[(&str, String)]) -> Line<'static> {
    let mut spans = vec![Span::raw("   ")];
    for (k, a) in h {
        spans.push(Span::styled(format!("[{k}]"), Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)));
        spans.push(Span::raw(format!(" {a}  ")));
    }
    Line::from(spans)
}

pub const TABS: [&str; 2] = ["queue", "projects"];

/// The tab bar; the active tab is reversed.
pub fn tabs_line(active: usize) -> Line<'static> {
    let mut spans = vec![];
    for (i, t) in TABS.iter().enumerate() {
        let style = if i == active {
            Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!(" {t} "), style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(" ⇥", Style::new().add_modifier(Modifier::DIM)));
    Line::from(spans)
}

/// A one-line text prompt in the footer: ` label › text▏`.
pub fn input_line(label: &str, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label} › "), Style::new().fg(Color::LightBlue)),
        Span::raw(text.to_string()),
        Span::raw("▏"),
    ])
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Pick {
    Project,
    Palaver,
    Meeting,
    Cluster,
    OpenProject,
}

/// A fuzzy picker over (label, value, detail). Project and cluster pickers offer "+ new …" as the last entry.
pub struct Picker {
    pub kind: Pick,
    pub title: String,
    pub key: String,     // ticket the picker acts on
    pub project: String, // cockpit project (for Palaver linking)
    pub then_meetings: bool,
    pub items: Vec<(String, String, String)>,
    pub query: String,
    pub sel: usize,
}

impl Picker {
    pub fn new(kind: Pick, title: String, items: Vec<(String, String, String)>) -> Self {
        Picker {
            kind,
            title,
            key: String::new(),
            project: String::new(),
            then_meetings: false,
            items,
            query: String::new(),
            sel: 0,
        }
    }

    pub fn shown(&self) -> Vec<(String, String, String)> {
        let q = self.query.to_lowercase();
        let mut v: Vec<_> = self.items.iter().filter(|(l, _, _)| fuzzy(&q, l)).cloned().collect();
        let creates = matches!(self.kind, Pick::Project | Pick::Cluster);
        if creates && !q.is_empty() && !self.items.iter().any(|(_, v, _)| v.to_lowercase() == q) {
            let what = if self.kind == Pick::Project { "project" } else { "cluster" };
            v.push((format!("+ new {what} \"{}\"", self.query), self.query.clone(), String::new()));
        }
        v
    }

    pub fn choice(&self) -> Option<String> {
        self.shown().get(self.sel).map(|x| x.1.clone())
    }
}

#[derive(PartialEq, Debug)]
pub enum PickerKey {
    Nothing,
    Close,
    Choose,
    /// ctrl-a in the meeting picker: let Claude pick.
    AutoLink,
}

/// Moves the cursor or edits the query; the caller acts on the returned event.
pub fn picker_key(p: &mut Picker, k: KeyEvent) -> PickerKey {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc => return PickerKey::Close,
        KeyCode::Enter => return PickerKey::Choose,
        KeyCode::Down => p.sel += 1,
        KeyCode::Char('n') if ctrl => p.sel += 1,
        KeyCode::Up => p.sel = p.sel.saturating_sub(1),
        KeyCode::Char('p') if ctrl => p.sel = p.sel.saturating_sub(1),
        KeyCode::Char('a') if ctrl && p.kind == Pick::Meeting => return PickerKey::AutoLink,
        KeyCode::Backspace => {
            p.query.pop();
            p.sel = 0;
        }
        KeyCode::Char(c) if !ctrl => {
            p.query.push(c);
            p.sel = 0;
        }
        _ => {}
    }
    let n = p.shown().len();
    p.sel = p.sel.min(n.saturating_sub(1));
    PickerKey::Nothing
}

pub fn render_picker(f: &mut Frame, main: Rect, p: &Picker) {
    let [area] = Layout::horizontal([Constraint::Percentage(90)]).flex(Flex::Center).areas(main);
    let [area] = Layout::vertical([Constraint::Percentage(80)]).flex(Flex::Center).areas(area);
    f.render_widget(Clear, area);
    let hint =
        if p.kind == Pick::Meeting { " enter link · ctrl-a let Claude pick · esc " } else { " enter choose · esc " };
    let block = Block::bordered().title(format!(" {} ", p.title)).title_bottom(hint);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [q, l, d] = Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(4)]).areas(inner);
    f.render_widget(Paragraph::new(format!("› {}▏", p.query)), q);
    let shown = p.shown();
    let items: Vec<ListItem> = shown.iter().map(|(lab, _, _)| ListItem::new(lab.clone())).collect();
    let mut ls = ListState::default();
    ls.select(if shown.is_empty() { None } else { Some(p.sel.min(shown.len() - 1)) });
    f.render_stateful_widget(
        List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        l,
        &mut ls,
    );
    let detail = ls.selected().and_then(|i| shown.get(i)).map(|x| x.2.clone()).unwrap_or_default();
    f.render_widget(
        Paragraph::new(detail).style(Style::new().add_modifier(Modifier::DIM)).wrap(Wrap { trim: true }),
        d,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<(String, String, String)> {
        names.iter().map(|n| (format!("◆ {n}"), n.to_string(), String::new())).collect()
    }

    #[test]
    fn fuzzy_is_a_case_insensitive_subsequence() {
        assert!(fuzzy("dmo", "Demo project"));
        assert!(fuzzy("", "anything"));
        assert!(!fuzzy("omd", "demo"));
    }

    #[test]
    fn new_entry_comes_last_and_only_without_an_exact_match() {
        let mut p = Picker::new(Pick::Project, "p".into(), items(&["Demo", "Docs"]));
        p.query = "do".into();
        let s = p.shown();
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].1.as_str(), s[1].1.as_str()), ("Demo", "Docs"));
        assert_eq!(s[2].0, "+ new project \"do\"");
        p.query = "demo".into();
        assert!(p.shown().iter().all(|(l, _, _)| !l.starts_with("+ new")));
        // a meeting picker never offers "+ new"
        let mut m = Picker::new(Pick::Meeting, "m".into(), items(&["a"]));
        m.query = "zzz".into();
        assert!(m.shown().is_empty());
        // clusters say cluster
        let mut c = Picker::new(Pick::Cluster, "c".into(), vec![]);
        c.query = "ui".into();
        assert_eq!(c.shown()[0].0, "+ new cluster \"ui\"");
    }

    #[test]
    fn waiting_times() {
        assert_eq!(ago(5), "5s");
        assert_eq!(ago(125), "2m");
        assert_eq!(ago(3900), "1h05m");
        assert_eq!(ago(200_000), "2d");
        assert_eq!(ago(-4), "0s");
    }
}
