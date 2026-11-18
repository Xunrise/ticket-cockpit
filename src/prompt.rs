//! Reading Claude's prompt from the screen so replies are never typed into a draft or a question menu.
use crate::tmux::{PaneRef, Tmux};
use anyhow::Result;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptState {
    /// A question menu ("Enter to select") is open.
    pub menu: bool,
    /// The user's own text in the input box; Claude's dim ghost suggestion is not a draft.
    pub draft: String,
    /// Vim editor mode is showing INSERT.
    pub insert: bool,
}

/// Why a reply was not delivered.
#[derive(Debug, Clone, PartialEq)]
pub enum SendRefused {
    Question,
    Draft(String),
    /// Typed, but the text is still in the prompt after Enter.
    NotTaken,
    NoPane,
    BadKey(String),
}

impl fmt::Display for SendRefused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SendRefused::Question => write!(f, "Claude is showing a question: answer with 1-4 or a, or attach"),
            SendRefused::Draft(d) => {
                let d: String = d.chars().take(40).collect();
                write!(f, "the prompt already has text in it (\"{d}\"): attach to send or clear it")
            }
            SendRefused::NotTaken => write!(f, "typed it, but Claude did not take it: attach and press Enter"),
            SendRefused::NoPane => write!(f, "no live tmux pane for this session"),
            SendRefused::BadKey(k) => write!(f, "unsupported key {k}"),
        }
    }
}

impl std::error::Error for SendRefused {}

fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The text after `❯` on a raw line (`after` is everything past the marker), and whether it is ghost text: every visible character
/// is dim, apart from the cursor (inverse video).
fn prompt_text(after: &str) -> (String, bool) {
    let (mut dim, mut inverse) = (false, false);
    let (mut text, mut visible, mut all_dim) = (String::new(), 0, true);
    let mut it = after.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            let mut params = String::new();
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    if d == 'm' {
                        for p in params.split(';') {
                            match p {
                                "" | "0" => (dim, inverse) = (false, false),
                                "2" => dim = true,
                                "22" => dim = false,
                                "7" => inverse = true,
                                "27" => inverse = false,
                                _ => {}
                            }
                        }
                    }
                    break;
                }
                params.push(d);
            }
            continue;
        }
        text.push(c);
        if !c.is_whitespace() && !inverse {
            visible += 1;
            all_dim &= dim;
        }
    }
    (text, visible > 0 && all_dim)
}

pub fn parse(raw: &str) -> PromptState {
    let screen = strip_sgr(raw);
    let lines: Vec<&str> = screen.lines().collect();
    let raws: Vec<&str> = raw.lines().collect();
    let is_rule = |l: &str| l.starts_with(&"─".repeat(10));
    let rules: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| is_rule(l)).map(|(i, _)| i).collect();
    let mut draft = String::new();
    if rules.len() >= 2 {
        let (top, bottom) = (rules[rules.len() - 2], rules[rules.len() - 1]);
        // the input box sits between the last two rules; earlier "❯ …" lines are past messages
        if let Some(first) = (top + 1..bottom).find(|&i| lines[i].starts_with('❯')) {
            let mut parts = vec![];
            for (i, raw_line) in raws.iter().enumerate().take(bottom).skip(first) {
                let after = if i == first { raw_line.split_once('❯').map(|x| x.1).unwrap_or("") } else { raw_line };
                let (text, ghost) = prompt_text(after);
                if !ghost && !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
            draft = parts.join("\n");
        }
    }
    PromptState { menu: screen.contains("Enter to select"), draft, insert: screen.contains("-- INSERT --") }
}

/// The screen operations safe sending needs; implemented for tmux and, in tests, by a fake.
pub trait Screen {
    fn raw(&self) -> Result<String>;
    fn literal(&self, text: &str) -> Result<()>;
    fn keys(&self, keys: &[&str]) -> Result<()>;
}

pub struct TmuxPane<'a> {
    pub tmux: &'a Tmux,
    pub pane: PaneRef,
}

impl Screen for TmuxPane<'_> {
    fn raw(&self) -> Result<String> {
        self.tmux.capture(&self.pane, true, None)
    }
    fn literal(&self, text: &str) -> Result<()> {
        self.tmux.send_literal(&self.pane, text)
    }
    fn keys(&self, keys: &[&str]) -> Result<()> {
        self.tmux.send_keys(&self.pane, keys)
    }
}

pub struct Timing {
    pub after_typing: Duration,
    pub after_enter: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { after_typing: Duration::from_millis(150), after_enter: Duration::from_millis(400) }
    }
}

/// Types `text` and presses Enter, after checking the screen: never into a question, never onto a draft.
/// In vim editor mode, enters insert mode first. Checks again afterwards and reports a reply that is still in the prompt.
pub fn send_text(screen: &dyn Screen, text: &str, vim: bool, timing: &Timing) -> Result<()> {
    let st = parse(&screen.raw()?);
    if st.menu {
        return Err(SendRefused::Question.into());
    }
    if !st.draft.is_empty() {
        return Err(SendRefused::Draft(st.draft).into());
    }
    if vim && !st.insert {
        screen.keys(&["i"])?; // NORMAL mode would read the text as commands
    }
    screen.literal(text)?;
    std::thread::sleep(timing.after_typing);
    screen.keys(&["Enter"])?;
    std::thread::sleep(timing.after_enter);
    if !parse(&screen.raw()?).draft.is_empty() {
        return Err(SendRefused::NotTaken.into());
    }
    Ok(())
}

pub const ALLOWED_KEYS: [&str; 8] = ["1", "2", "3", "4", "Up", "Down", "Enter", "Escape"];
const EXTRA_KEYS: [&str; 2] = ["Tab", "BTab"];

pub fn send_keys(screen: &dyn Screen, keys: &[String]) -> Result<()> {
    for k in keys {
        if !ALLOWED_KEYS.contains(&k.as_str()) && !EXTRA_KEYS.contains(&k.as_str()) {
            return Err(SendRefused::BadKey(k.clone()).into());
        }
    }
    for k in keys {
        screen.keys(&[k])?;
    }
    Ok(())
}

/// True when `editorMode` is "vim" in Claude's settings.
pub fn vim_mode(settings_path: &std::path::Path) -> bool {
    std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["editorMode"] == "vim")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/prompt/{name}.txt", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn fixtures_are_classified() {
        let empty = parse(&fx("empty"));
        assert_eq!(empty, PromptState::default());
        // Claude's dim suggestion is not a draft, with or without the cursor highlighting its first letter
        assert_eq!(parse(&fx("ghost_cursor")).draft, "");
        assert_eq!(parse(&fx("ghost_dim")).draft, "");
        assert_eq!(parse(&fx("draft")).draft, "fix the login bug");
        assert_eq!(parse(&fx("draft_multiline")).draft, "fix the login bug\nand add a test");
        let q = parse(&fx("question"));
        assert!(q.menu);
        assert_eq!(q.draft, "", "an option line is not a draft");
        assert!(!parse(&fx("vim_normal")).insert);
        assert!(parse(&fx("vim_insert")).insert);
        assert_eq!(parse(&fx("vim_draft")).draft, "half a thought");
        // the past message above the box is never taken for a draft
        assert!(fx("empty").contains("earlier message"));
    }

    #[test]
    fn screens_without_a_box_are_empty() {
        assert_eq!(parse("just some output\n"), PromptState::default());
    }

    struct Fake {
        screens: RefCell<Vec<String>>,
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(screens: &[&str]) -> Self {
            Fake { screens: RefCell::new(screens.iter().rev().map(|s| fx(s)).collect()), log: RefCell::new(vec![]) }
        }
        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl Screen for Fake {
        fn raw(&self) -> Result<String> {
            let mut s = self.screens.borrow_mut();
            Ok(if s.len() > 1 { s.pop().unwrap() } else { s[0].clone() })
        }
        fn literal(&self, t: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("type:{t}"));
            Ok(())
        }
        fn keys(&self, k: &[&str]) -> Result<()> {
            self.log.borrow_mut().push(format!("keys:{}", k.join("+")));
            Ok(())
        }
    }

    fn t0() -> Timing {
        Timing { after_typing: Duration::ZERO, after_enter: Duration::ZERO }
    }

    fn refused(r: Result<()>) -> SendRefused {
        r.unwrap_err().downcast::<SendRefused>().unwrap()
    }

    #[test]
    fn sends_into_an_empty_prompt_and_over_a_ghost() {
        for first in ["empty", "ghost_cursor", "ghost_dim"] {
            let f = Fake::new(&[first, "empty"]);
            send_text(&f, "continue", false, &t0()).unwrap();
            assert_eq!(f.log(), ["type:continue", "keys:Enter"], "{first}");
        }
    }

    #[test]
    fn refuses_questions_and_drafts_without_typing() {
        let f = Fake::new(&["question"]);
        assert_eq!(refused(send_text(&f, "yes", false, &t0())), SendRefused::Question);
        assert!(f.log().is_empty());
        let f = Fake::new(&["draft"]);
        assert_eq!(refused(send_text(&f, "yes", false, &t0())), SendRefused::Draft("fix the login bug".into()));
        assert!(f.log().is_empty());
    }

    #[test]
    fn reports_not_taken_when_the_text_stays_in_the_prompt() {
        let f = Fake::new(&["empty", "draft"]);
        assert_eq!(refused(send_text(&f, "yes", false, &t0())), SendRefused::NotTaken);
        assert_eq!(f.log(), ["type:yes", "keys:Enter"]);
    }

    #[test]
    fn vim_mode_enters_insert_first_unless_already_there() {
        let f = Fake::new(&["vim_normal", "empty"]);
        send_text(&f, "go", true, &t0()).unwrap();
        assert_eq!(f.log(), ["keys:i", "type:go", "keys:Enter"]);
        let f = Fake::new(&["vim_insert", "empty"]);
        send_text(&f, "go", true, &t0()).unwrap();
        assert_eq!(f.log(), ["type:go", "keys:Enter"]);
        let f = Fake::new(&["vim_normal", "empty"]);
        send_text(&f, "go", false, &t0()).unwrap();
        assert_eq!(f.log(), ["type:go", "keys:Enter"], "no vim setting, no i");
    }

    #[test]
    fn keys_are_allow_listed() {
        let f = Fake::new(&["question"]);
        send_keys(&f, &["2".into(), "Enter".into()]).unwrap();
        assert_eq!(f.log(), ["keys:2", "keys:Enter"]);
        assert_eq!(refused(send_keys(&f, &["q".into()])), SendRefused::BadKey("q".into()));
    }

    #[test]
    fn refusal_messages_match_what_the_queue_shows() {
        assert_eq!(SendRefused::Question.to_string(), "Claude is showing a question: answer with 1-4 or a, or attach");
        assert!(SendRefused::Draft("x".repeat(60)).to_string().contains(&"x".repeat(40)));
        assert!(!SendRefused::Draft("x".repeat(60)).to_string().contains(&"x".repeat(41)));
    }

    #[test]
    fn vim_setting_is_read_from_claude_settings() {
        let d = crate::db::tempdir::Dir::new();
        let p = d.path().join("s.json");
        std::fs::write(&p, r#"{"editorMode":"vim"}"#).unwrap();
        assert!(vim_mode(&p));
        std::fs::write(&p, r#"{"editorMode":"normal"}"#).unwrap();
        assert!(!vim_mode(&p));
        assert!(!vim_mode(&d.path().join("missing.json")));
    }

    #[test]
    fn real_tmux_capture_keeps_what_the_parser_needs() {
        use crate::tmux::{testutil, Server, SESSION};
        let Some(g) = testutil::guard() else { return };
        let t = &g.0;
        let dir = format!("{}/tests/fixtures/prompt", env!("CARGO_MANIFEST_DIR"));
        for (name, draft, menu) in [
            ("empty", "", false),
            ("ghost_cursor", "", false),
            ("ghost_dim", "", false),
            ("draft", "fix the login bug", false),
            ("question", "", true),
        ] {
            let w = format!("w-{name}");
            let cmd = format!("cat {dir}/{name}.txt; sleep 30");
            t.run(
                Server::Cockpit,
                &["new-session", "-d", "-s", &format!("{SESSION}-{name}"), "-n", &w, "-x", "100", "-y", "30", &cmd],
            )
            .unwrap();
            let id = t
                .run(Server::Cockpit, &["list-panes", "-t", &format!("={SESSION}-{name}"), "-F", "#{pane_id}"])
                .unwrap();
            let p = PaneRef::cockpit(&id);
            assert!(testutil::wait_until(|| t.capture(&p, false, None).unwrap().contains("─────")), "{name}");
            let st = parse(&t.capture(&p, true, None).unwrap());
            assert_eq!((st.draft.as_str(), st.menu), (draft, menu), "{name}");
        }
    }
}
