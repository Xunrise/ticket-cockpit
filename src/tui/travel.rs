//! Where the queue sits. It travels: it joins the window of the selected ticket (Claude pane + service
//! panes) on the left, follows the window the user switched to, and rests in the home window, whose right
//! pane (the slot) shows details when nothing tmux-backed is selected.
use super::queue::{border_hints, window_label, Row};
use crate::tmux::{Server, Tmux, HOME_WINDOW, SESSION};
use anyhow::{anyhow, Result};

const CK: Server = Server::Cockpit;
pub const NO_SESSION: &str = "no tmux session selected";
const SLOT_EMPTY: &str = "\n  no live tmux session selected\n";
/// Border of every pane in a visited window: queue, named service pane, the active Claude pane (back-to-queue
/// hint) or the label with the context's keys.
pub const BORDER_FORMAT: &str = "#{?@tk_queue, queue ,#{?@tk_name, #{@tk_name} ,#{?pane_active, #{s/  │.*//:@tk_label}  │  ⇧esc back to queue , #{@tk_label} }}}";

/// The user switched windows themselves (picker, mouse): the queue follows.
pub fn follows(at_win: &str, looking: &str) -> bool {
    !looking.is_empty() && looking != at_win
}

/// The queue goes to the window of the selection, unless it was left on a window without a row of its own
/// (e.g. home) and the selection has not changed since. The projects tab always follows its selection.
pub fn should_join(want: &str, at_win: &str, projects_tab: bool, pinned: Option<&str>, sel_id: &str) -> bool {
    want != at_win && (projects_tab || pinned != Some(sel_id))
}

#[derive(Default, Debug)]
pub struct SyncOut {
    /// The row to select: the user moved to that session's window.
    pub select: Option<String>,
    /// Something moved; refresh data right away.
    pub refresh: bool,
}

pub struct Travel {
    tmux: Tmux,
    me: String,
    slot: String,
    home_win: String,
    at_win: String,
    pinned: Option<String>,
    slot_key: String,
    border_label: String,
    /// The Claude pane shown next to the queue (its id on the cockpit's server).
    pub shown: Option<String>,
}

impl Travel {
    /// Marks this pane as the queue, opens the slot to its right, names the home window `cockpit` and puts it first.
    pub fn start(tmux: Tmux, me: &str) -> Result<Travel> {
        let slot = tmux
            .run(
                CK,
                &[
                    "split-window",
                    "-h",
                    "-d",
                    "-l",
                    "62%",
                    "-t",
                    me,
                    "-P",
                    "-F",
                    "#{pane_id}",
                    &format!("printf '{}'; exec tail -f /dev/null", SLOT_EMPTY.replace('\n', "\\n")),
                ],
            )
            .map_err(|e| anyhow!("cannot open the slot pane: {e}"))?;
        let _ = tmux.run(CK, &["set", "-p", "-t", me, "@tk_queue", "1"]);
        // the launcher compares this with its own build to replace a queue left over from an older one
        let _ = tmux.run(CK, &["set", "-p", "-t", me, "@tk_build", &crate::util::build_id()]);
        let home_win = tmux.quiet(CK, &["display", "-p", "-t", me, "#{window_id}"]);
        let t = Travel {
            tmux,
            me: me.into(),
            slot,
            at_win: home_win.clone(),
            home_win,
            pinned: None,
            slot_key: String::new(),
            border_label: String::new(),
            shown: None,
        };
        t.label_window(&t.slot, NO_SESSION);
        let _ = t.tmux.run(CK, &["rename-window", "-t", &t.home_win, HOME_WINDOW]);
        // pinned first in the picker (base-index is 1)
        let _ = t.tmux.run(CK, &["move-window", "-d", "-s", &t.home_win, "-t", &format!("={SESSION}:0")]);
        Ok(t)
    }

    pub fn label_window(&self, target: &str, label: &str) {
        let t = &self.tmux;
        let _ = t.run(CK, &["set", "-w", "-t", target, "pane-border-status", "bottom"]);
        let _ = t.run(CK, &["set", "-w", "-t", target, "pane-border-format", BORDER_FORMAT]);
        let _ = t.run(CK, &["set", "-w", "-t", target, "@tk_label", label]);
    }

    fn win_of(&self, pane: &str) -> String {
        self.tmux.quiet(CK, &["display", "-p", "-t", pane, "#{window_id}"])
    }

    fn join(&self, win: &str, width: i64, detached: bool) -> bool {
        let w = format!("{width}%");
        let target = format!("{win}.{{top-left}}");
        let mut a = vec!["join-pane"];
        if detached {
            a.push("-d");
        }
        a.extend(["-f", "-h", "-b", "-l", &w, "-s", &self.me, "-t", &target]);
        self.tmux.run(CK, &a).is_ok()
    }

    /// `@tk_select`, set by cogitator's bar: the session to select. Cleared once read.
    pub fn pending_select(&self) -> Option<String> {
        let v = self.tmux.quiet(CK, &["show", "-gv", "@tk_select"]);
        if v.is_empty() {
            return None;
        }
        let _ = self.tmux.run(CK, &["set", "-gu", "@tk_select"]);
        Some(v)
    }

    /// Puts the queue where the selection (`focus`) lives, following the user's own window switches.
    pub fn sync(
        &mut self,
        focus: Option<&Row>,
        rows: &[Row],
        sel_id: &str,
        projects_tab: bool,
        width: i64,
        replies: &[(char, String)],
    ) -> SyncOut {
        let mut out = SyncOut::default();
        // the daemon may have moved the queue home when it closed the window the queue sat in
        let mine = self.win_of(&self.me);
        if !mine.is_empty() && mine != self.at_win {
            self.at_win = mine;
        }
        let pane = focus.and_then(|r| r.cockpit_pane.clone());
        let want =
            pane.as_deref().map(|p| self.win_of(p)).filter(|w| !w.is_empty()).unwrap_or_else(|| self.home_win.clone());
        let looking = self.tmux.quiet(CK, &["display", "-p", "-t", &format!("={SESSION}:"), "#{window_id}"]);
        if follows(&self.at_win, &looking) {
            if self.join(&looking, width, true) {
                self.at_win = looking.clone();
                self.border_label.clear();
                let here = rows.iter().find(|r| r.cockpit_pane.as_deref().is_some_and(|p| self.win_of(p) == looking));
                match here {
                    Some(r) => {
                        out.select = Some(r.id.clone());
                        self.pinned = None;
                    }
                    None => self.pinned = Some(sel_id.to_string()), // e.g. home: stay until another row is picked
                }
                self.label_window(&looking, here.map_or(NO_SESSION, |r| r.label.as_str()));
                out.refresh = true;
            }
            // the selection changes on the next tick; deciding with the old focus would pull the user back
            return out;
        }
        if should_join(&want, &self.at_win, projects_tab, self.pinned.as_deref(), sel_id)
            && self.join(&want, width, false)
        {
            let label = focus.filter(|_| want != self.home_win).map_or(NO_SESSION, |r| r.label.as_str());
            self.label_window(&want, label);
            let _ = self.tmux.run(CK, &["select-window", "-t", &want]);
            let _ = self.tmux.run(CK, &["select-pane", "-t", &self.me]);
            self.at_win = want.clone();
            self.border_label.clear();
        }
        // picking another row ends the stay on a window without a row of its own
        if self.pinned.as_deref() != Some(sel_id) {
            self.pinned = None;
        }
        self.shown = pane.filter(|_| want == self.at_win && self.at_win != self.home_win);
        if let Some(r) = focus.filter(|_| self.shown.is_some()) {
            let label = window_label(&r.label, &border_hints(r, replies));
            if label != self.border_label {
                let _ = self.tmux.run(CK, &["set", "-w", "-t", &self.at_win, "@tk_label", &label]);
                self.border_label = label;
            }
        }
        out
    }

    /// Moves the cursor into the shown Claude pane (no zoom).
    pub fn attach(&self) -> bool {
        match &self.shown {
            Some(p) => self.tmux.run(CK, &["select-pane", "-t", p]).is_ok(),
            None => false,
        }
    }

    /// Shows `text` in the home window's right pane when `key` changed.
    pub fn show_slot(&mut self, key: &str, text: impl FnOnce() -> String) {
        if key == self.slot_key {
            return;
        }
        self.slot_key = key.to_string();
        let t = &self.tmux;
        let width = t
            .quiet(CK, &["display", "-p", "-t", &self.slot, "#{pane_width}"])
            .parse::<usize>()
            .unwrap_or(100)
            .saturating_sub(2)
            .max(20);
        let file = std::env::temp_dir().join(format!("cockpit-slot-{}.txt", t.cockpit_sock));
        let _ = std::fs::write(&file, wrap_ansi(&text(), width));
        let _ = t.run(
            CK,
            &["respawn-pane", "-k", "-t", &self.slot, &format!("cat '{}'; exec tail -f /dev/null", file.display())],
        );
    }

    /// Detaches the terminals looking at this queue, not every client of the session.
    pub fn detach_clients(&self) {
        for l in self.tmux.quiet(CK, &["list-clients", "-F", "#{client_name} #{pane_id}"]).lines() {
            if let Some((client, pane)) = l.split_once(' ') {
                if pane == self.me {
                    let _ = self.tmux.run(CK, &["detach-client", "-t", client]);
                }
            }
        }
    }

    /// Q: back to the home window, slot closed.
    pub fn stop(&self) {
        if self.at_win != self.home_win {
            let _ = self.tmux.run(
                CK,
                &["join-pane", "-f", "-h", "-b", "-s", &self.me, "-t", &format!("{}.{{top-left}}", self.home_win)],
            );
        }
        let _ = self.tmux.run(CK, &["kill-pane", "-t", &self.slot]);
    }

    pub fn home_win(&self) -> &str {
        &self.home_win
    }
    pub fn at_win(&self) -> &str {
        &self.at_win
    }
}

/// Wraps to `width` columns keeping each line's indent; SGR codes do not count towards the width.
pub fn wrap_ansi(text: &str, width: usize) -> String {
    let visible = |w: &str| {
        let (mut n, mut esc) = (0, false);
        for c in w.chars() {
            if c == '\x1b' {
                esc = true;
            } else if esc {
                esc = c != 'm';
            } else {
                n += 1;
            }
        }
        n
    };
    let mut out = String::new();
    for line in text.lines() {
        let indent: String = line.chars().take_while(|c| *c == ' ').collect();
        let (mut cur, mut len) = (indent.clone(), indent.len());
        for word in line.trim_start().split(' ') {
            let w = visible(word);
            if len + w + 1 > width && len > indent.len() {
                out += &cur;
                out += "\n";
                cur = indent.clone();
                len = indent.len();
            }
            if len > indent.len() {
                cur.push(' ');
                len += 1;
            }
            cur += word;
            len += w;
        }
        out += &cur;
        out += "\n";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::testutil;

    #[test]
    fn decisions() {
        assert!(follows("@1", "@2"));
        assert!(!follows("@1", "@1"));
        assert!(!follows("@1", ""));
        // already there
        assert!(!should_join("@1", "@1", false, None, "a"));
        // left on home by the user: stays until the selection changes
        assert!(!should_join("@2", "@1", false, Some("a"), "a"));
        assert!(should_join("@2", "@1", false, Some("a"), "b"));
        assert!(should_join("@2", "@1", false, None, "a"));
        // the projects tab always follows its selection
        assert!(should_join("@2", "@1", true, Some("a"), "a"));
    }

    #[test]
    fn wrapping_keeps_indent_and_ignores_escapes() {
        assert_eq!(wrap_ansi("  aaa bbb ccc", 9), "  aaa bbb\n  ccc\n");
        assert_eq!(wrap_ansi("\x1b[1mab\x1b[0m cd", 5), "\x1b[1mab\x1b[0m cd\n");
        assert_eq!(wrap_ansi("a\n\nb", 10), "a\n\nb\n");
    }

    fn row(id: &str, pane: &str) -> Row {
        Row {
            section: 1,
            id: id.into(),
            label: id.into(),
            sub: String::new(),
            place: String::new(),
            since: 0,
            msg: String::new(),
            pane: true,
            cockpit_pane: Some(pane.into()),
            key: id.into(),
            project: None,
        }
    }

    #[test]
    fn the_queue_travels_and_follows_the_user() {
        let Some(g) = testutil::guard() else { return };
        let t = &g.0;
        t.run(CK, &["new-session", "-d", "-s", SESSION, "-x", "120", "-y", "40", "-n", "q", "cat"]).unwrap();
        let me = t.run(CK, &["list-panes", "-t", SESSION, "-F", "#{pane_id}"]).unwrap();
        for w in ["DEM-1", "DEM-2"] {
            t.run(CK, &["new-window", "-d", "-t", &format!("={SESSION}:"), "-n", w, "cat"]).unwrap();
        }
        let pane_of =
            |w: &str| t.run(CK, &["list-panes", "-t", &format!("={SESSION}:{w}"), "-F", "#{pane_id}"]).unwrap();
        let win_of = |p: &str| t.quiet(CK, &["display", "-p", "-t", p, "#{window_id}"]);
        let (p1, p2) = (pane_of("DEM-1"), pane_of("DEM-2"));
        let rows = vec![row("a", &p1), row("b", &p2)];
        let mut tr = Travel::start(t.clone(), &me).unwrap();
        assert_eq!(t.quiet(CK, &["display", "-p", "-t", &me, "#{window_name}"]), HOME_WINDOW);
        let home = tr.home_win().to_string();
        assert_eq!(
            t.quiet(CK, &["display", "-p", "-t", &me, "#{@tk_queue}-#{@tk_build}"]),
            format!("1-{}", crate::util::build_id())
        );
        let q = [('c', "continue".to_string())];

        // selecting a row joins the queue to its window, on the left, full height
        let out = tr.sync(Some(&rows[0]), &rows, "a", false, 38, &q);
        assert!(out.select.is_none());
        assert_eq!(win_of(&me), win_of(&p1));
        assert_eq!(t.quiet(CK, &["display", "-p", "-t", &me, "#{pane_left}-#{pane_height}"]), "0-39");
        assert_eq!(t.quiet(CK, &["display", "-p", "-t", &format!("={SESSION}:"), "#{window_id}"]), win_of(&p1));
        assert_eq!(tr.shown.as_deref(), Some(p1.as_str()));
        let label = t.quiet(CK, &["display", "-p", "-t", &p1, "#{@tk_label}"]);
        assert!(label.starts_with("#[bold]a#[default]  │  ") && label.contains("reply"), "{label}");
        assert_eq!(t.quiet(CK, &["show", "-wv", "-t", &p1, "pane-border-status"]), "bottom");
        // nothing moves when nothing changed
        let before = tr.at_win().to_string();
        assert!(tr.sync(Some(&rows[0]), &rows, "a", false, 38, &q).select.is_none());
        assert_eq!(tr.at_win(), before);

        // the user switches to b's window: the queue follows and selects b, without being pulled back
        t.run(CK, &["select-window", "-t", &win_of(&p2)]).unwrap();
        let out = tr.sync(Some(&rows[0]), &rows, "a", false, 38, &q);
        assert_eq!(out.select.as_deref(), Some("b"));
        assert_eq!(win_of(&me), win_of(&p2));
        let out = tr.sync(Some(&rows[1]), &rows, "b", false, 38, &q);
        assert!(out.select.is_none());
        assert_eq!(win_of(&me), win_of(&p2));

        // the user goes home: the queue stays there until another row is picked
        t.run(CK, &["select-window", "-t", &home]).unwrap();
        tr.sync(Some(&rows[1]), &rows, "b", false, 38, &q);
        assert_eq!(win_of(&me), home);
        tr.sync(Some(&rows[1]), &rows, "b", false, 38, &q);
        assert_eq!(win_of(&me), home);
        assert!(tr.shown.is_none());
        tr.sync(Some(&rows[0]), &rows, "a", false, 38, &q);
        assert_eq!(win_of(&me), win_of(&p1));
        // and going back to the row that was pinned travels again
        tr.sync(Some(&rows[1]), &rows, "b", false, 38, &q);
        assert_eq!(win_of(&me), win_of(&p2));

        // @tk_select from cogitator's bar
        t.run(CK, &["set", "-g", "@tk_select", "b"]).unwrap();
        assert_eq!(tr.pending_select().as_deref(), Some("b"));
        assert_eq!(tr.pending_select(), None);

        // Q: back home, slot gone
        tr.stop();
        assert_eq!(win_of(&me), home);
        assert_eq!(t.quiet(CK, &["list-panes", "-t", &home, "-F", "#{pane_id}"]).lines().count(), 1);
    }
}
