//! Close-out of a ticket: wrap up with its session, close now, close the window but keep the ticket, or drop it.
use crate::ctx::Ctx;
use crate::prompt::{self, Timing, TmuxPane};
use crate::tmux::{Server, Tmux, HOME_WINDOW, SESSION};
use crate::util::now_iso;
use crate::{panes, sessions, settings, tickets};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const CK: Server = Server::Cockpit;

pub fn wrap_text(key: &str) -> String {
    format!("Wrap up {key}: finish what this ticket still needs (commit if appropriate), then close it with: cockpit close {key} --summary \"<one paragraph>\"")
}

fn end_session(conn: &Connection, session: Option<&str>) -> Result<()> {
    if let Some(s) = session {
        let now = now_iso();
        conn.execute(
            "UPDATE sessions SET state='ended', ended=COALESCE(ended,?), state_at=? WHERE id=?",
            params![now, now, s],
        )?;
    }
    Ok(())
}

/// Stops the ticket's service panes, moves the queue home if it sits in the window, then kills the window.
fn close_window(c: &Ctx, conn: &Connection, key: &str) {
    let tmux = &c.tmux;
    let target = format!("={SESSION}:{key}");
    if let Ok(w) = tmux.run(CK, &["display", "-p", "-t", &target, "#{window_id}"]) {
        for p in panes::panes_in(tmux, &w).unwrap_or_default().iter().filter(|p| p.name.is_some()) {
            let _ = tmux.run(CK, &["kill-pane", "-t", &p.id]);
        }
    }
    queue_home(tmux, conn, &target);
    let _ = tmux.run(CK, &["kill-window", "-t", &target]);
}

/// The queue travels between windows; one in the window about to die would die with it.
fn queue_home(tmux: &Tmux, conn: &Connection, window: &str) {
    let listing = tmux.quiet(CK, &["list-panes", "-t", window, "-F", "#{pane_id}\t#{@tk_queue}"]);
    let Some(queue) = listing.lines().filter_map(|l| l.split_once('\t')).find(|(_, q)| *q == "1").map(|(p, _)| p)
    else {
        return;
    };
    let windows = tmux.quiet(CK, &["list-windows", "-t", &format!("={SESSION}"), "-F", "#{window_id}\t#{window_name}"]);
    let Some((home, _)) = windows.lines().filter_map(|l| l.split_once('\t')).find(|(_, n)| *n == HOME_WINDOW) else {
        return;
    };
    let width = settings::get(conn, "queue_width").ok().and_then(|v| v.as_i64()).unwrap_or(38);
    let _ = tmux.run(
        CK,
        &[
            "join-pane",
            "-d",
            "-f",
            "-h",
            "-b",
            "-l",
            &format!("{width}%"),
            "-s",
            queue,
            "-t",
            &format!("{home}.{{top-left}}"),
        ],
    );
    let _ = tmux.run(CK, &["select-window", "-t", home]);
}

/// `mode`: wrap | now | keep | drop. A refused wrap-up (draft, open question) is a normal result, not an error.
pub fn close_out(c: &Ctx, key: &str, mode: &str, summary: &str) -> Result<Value> {
    let conn = c.db()?;
    let t = tickets::get(&conn, key)?;
    let session = t.session.as_deref();
    match mode {
        "wrap" => {
            let Some(pane) = session.and_then(|s| sessions::live_pane(&conn, &c.tmux, s)) else {
                return Ok(
                    json!({"ok": false, "refused": format!("{key} has no live session to wrap up (use close now or drop)")}),
                );
            };
            let screen = TmuxPane { tmux: &c.tmux, pane };
            if let Err(e) =
                prompt::send_text(&screen, &wrap_text(key), prompt::vim_mode(&c.claude_settings), &Timing::default())
            {
                return Ok(json!({"ok": false, "refused": e.to_string()}));
            }
            conn.execute(
                "INSERT OR REPLACE INTO close_requests(ticket,session,at) VALUES(?,?,?)",
                params![key, session, now_iso()],
            )?;
            tickets::event(&conn, key, None, "comment", "close requested: waiting for the session to wrap up")?;
            Ok(
                json!({"ok": true, "mode": mode, "msg": format!("asked {key}'s session to wrap up; the window closes when it is done")}),
            )
        }
        "now" => {
            tickets::close(&conn, key, session, summary)?;
            finish(c, &conn, key, session)?;
            Ok(json!({"ok": true, "mode": mode, "msg": format!("closed {key}")}))
        }
        "keep" | "drop" => {
            let status = if mode == "keep" { "todo" } else { "canceled" };
            tickets::update(&conn, key, json!({"status": status}).as_object().unwrap(), None)?;
            finish(c, &conn, key, session)?;
            let msg =
                if mode == "keep" { format!("{key} back to todo, window closed") } else { format!("{key} dropped") };
            Ok(json!({"ok": true, "mode": mode, "msg": msg}))
        }
        m => bail!("mode must be wrap|now|keep|drop, not {m}"),
    }
}

fn finish(c: &Ctx, conn: &Connection, key: &str, session: Option<&str>) -> Result<()> {
    end_session(conn, session)?;
    conn.execute("DELETE FROM close_requests WHERE ticket=?", [key])?;
    close_window(c, conn, key);
    Ok(())
}

/// Run by the daemon every 2 s: finishes the requests whose ticket is done and whose session stopped working.
pub fn finish_requested(c: &Ctx) -> Result<()> {
    let conn = c.db()?;
    let requests: Vec<(String, Option<String>)> = conn
        .prepare("SELECT ticket, session FROM close_requests")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (key, session) in requests {
        match tickets::find(&conn, &key)? {
            None => {
                conn.execute("DELETE FROM close_requests WHERE ticket=?", [&key])?;
            }
            Some(t) if t.status == "done" => {
                let state: Option<String> = match &session {
                    Some(s) => conn
                        .query_row("SELECT state FROM sessions WHERE id=? AND ended IS NULL", [s], |r| r.get(0))
                        .optional()?
                        .flatten(),
                    None => None,
                };
                // working or waiting on a permission: not finished yet
                if matches!(state.as_deref(), Some("working" | "needs")) {
                    continue;
                }
                finish(c, &conn, &key, session.as_deref())?;
                tickets::event(&conn, &key, None, "comment", "window closed after wrap-up")?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::tmux::testutil::{guard, wait_until, Guard};

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/prompt/{name}.txt", env!("CARGO_MANIFEST_DIR"))
    }

    /// A ticket with a claiming session whose window shows a Claude-like prompt screen (a private tmux).
    struct Rig {
        _d: Dir,
        g: Guard,
        c: Ctx,
        key: String,
    }

    fn rig(screen: &str) -> Option<Rig> {
        let g = guard()?;
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), g.0.clone());
        let conn = c.db().unwrap();
        conn.execute("INSERT INTO repos(name,path) VALUES('demo', ?)", [d.path().to_string_lossy()]).unwrap();
        let key =
            tickets::create(&conn, tickets::NewTicket { repo: "demo".into(), title: "T".into(), ..Default::default() })
                .unwrap();
        // the home window comes first, like the real queue's
        panes::cockpit_window(&c.tmux, HOME_WINDOW, "/tmp", &[], "sleep 600").unwrap();
        let cmd = format!("sh -c 'cat {}; exec cat' --", fixture(screen));
        panes::cockpit_window(&c.tmux, &key, "/tmp", &[], &cmd).unwrap();
        let pane = c.tmux.run(CK, &["display", "-p", "-t", &format!("={SESSION}:{key}"), "#{pane_id}"]).unwrap();
        conn.execute(
            "INSERT INTO sessions(id,repo,cwd,tmux_pane,state) VALUES('s1','demo','/tmp',?,'turn')",
            [format!("cockpit:{pane}")],
        )
        .unwrap();
        tickets::claim(&conn, &key, "s1").unwrap();
        assert!(wait_until(|| c
            .tmux
            .capture(&crate::tmux::PaneRef::cockpit(&pane), false, None)
            .is_ok_and(|s| s.contains("shortcuts"))));
        Some(Rig { _d: d, g, c, key })
    }

    impl Rig {
        fn window_open(&self) -> bool {
            panes::window_exists(&self.c.tmux, &self.key)
        }
        fn state(&self) -> (String, bool) {
            self.c
                .db()
                .unwrap()
                .query_row("SELECT state, ended IS NOT NULL FROM sessions WHERE id='s1'", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap()
        }
        fn requests(&self) -> i64 {
            self.c.db().unwrap().query_row("SELECT count(*) FROM close_requests", [], |r| r.get(0)).unwrap()
        }
        fn status(&self) -> String {
            tickets::get(&self.c.db().unwrap(), &self.key).unwrap().status
        }
    }

    #[test]
    fn close_now_marks_done_ends_the_session_and_kills_the_window() {
        let Some(r) = rig("empty") else { return };
        let out = close_out(&r.c, &r.key, "now", "all good").unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(r.status(), "done");
        assert_eq!(r.state(), ("ended".into(), true));
        assert!(!r.window_open());
        let ev = tickets::events(&r.c.db().unwrap(), &r.key).unwrap();
        assert!(ev.iter().any(|e| e["kind"] == "closed" && e["text"].as_str().unwrap().contains("all good")));
        drop(r.g);
    }

    #[test]
    fn keep_returns_the_ticket_to_todo_and_drop_cancels_it() {
        let Some(r) = rig("empty") else { return };
        close_out(&r.c, &r.key, "keep", "").unwrap();
        assert_eq!((r.status().as_str(), r.state().1, r.window_open()), ("todo", true, false));
        let Some(r) = rig("empty") else { return };
        close_out(&r.c, &r.key, "drop", "").unwrap();
        assert_eq!((r.status().as_str(), r.state().1, r.window_open()), ("canceled", true, false));
        assert!(close_out(&r.c, &r.key, "bogus", "").is_err());
    }

    #[test]
    fn a_wrap_refused_by_a_draft_records_nothing() {
        let Some(r) = rig("draft") else { return };
        let out = close_out(&r.c, &r.key, "wrap", "").unwrap();
        assert_eq!(out["ok"], false);
        assert!(out["refused"].as_str().unwrap().contains("fix the login bug"), "{out}");
        assert_eq!(r.requests(), 0);
        assert!(r.window_open());
        // a later unrelated close is not affected by a phantom request
        tickets::close(&r.c.db().unwrap(), &r.key, None, "x").unwrap();
        finish_requested(&r.c).unwrap();
        assert!(r.window_open());
    }

    #[test]
    fn a_wrap_without_a_live_session_is_refused() {
        let Some(r) = rig("empty") else { return };
        r.c.db().unwrap().execute("UPDATE sessions SET tmux_pane='cockpit:%999'", []).unwrap();
        assert_eq!(close_out(&r.c, &r.key, "wrap", "").unwrap()["ok"], false);
        assert_eq!(r.requests(), 0);
    }

    #[test]
    fn the_watcher_closes_only_after_done_and_not_working() {
        let Some(r) = rig("empty") else { return };
        let out = close_out(&r.c, &r.key, "wrap", "").unwrap();
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(r.requests(), 1);
        let conn = r.c.db().unwrap();
        // the session wraps up: working, ticket not done yet
        conn.execute("UPDATE sessions SET state='working'", []).unwrap();
        finish_requested(&r.c).unwrap();
        assert!(r.window_open());
        // closed by the session while its turn is still running
        tickets::close(&conn, &r.key, None, "wrapped").unwrap();
        finish_requested(&r.c).unwrap();
        assert!(r.window_open(), "still working");
        // a service pane runs in the window; it goes with it
        panes::run(&r.c.tmux, &panes::Sel { key: Some(r.key.clone()), caller_pane: None }, "svc", "sleep 600", None)
            .unwrap();
        // the turn ends
        conn.execute("UPDATE sessions SET state='turn'", []).unwrap();
        finish_requested(&r.c).unwrap();
        assert!(!r.window_open());
        assert_eq!((r.state(), r.requests()), (("ended".into(), true), 0));
        let ev = tickets::events(&conn, &r.key).unwrap();
        assert!(ev.iter().any(|e| e["text"] == "window closed after wrap-up"));
    }

    #[test]
    fn the_queue_moves_home_before_its_window_is_killed() {
        let Some(r) = rig("empty") else { return };
        let t = &r.c.tmux;
        // the home window keeps a slot pane; the queue pane (a second one) visits the ticket's window
        let home = format!("={SESSION}:{HOME_WINDOW}");
        let q = t.run(CK, &["split-window", "-d", "-t", &home, "-P", "-F", "#{pane_id}", "sleep 600"]).unwrap();
        t.run(CK, &["set", "-p", "-t", &q, "@tk_queue", "1"]).unwrap();
        t.run(CK, &["join-pane", "-d", "-h", "-s", &q, "-t", &format!("={SESSION}:{}", r.key)]).unwrap();
        let before = t.quiet(CK, &["list-panes", "-t", &format!("={SESSION}:{}", r.key), "-F", "#{pane_id}"]);
        assert!(before.lines().any(|p| p == q));
        close_out(&r.c, &r.key, "now", "").unwrap();
        assert!(!r.window_open());
        let panes = t.quiet(CK, &["list-panes", "-a", "-F", "#{pane_id} #{@tk_queue}"]);
        assert!(panes.lines().any(|l| l == format!("{q} 1")), "queue survived: {panes}");
        let home_panes = t.quiet(CK, &["list-panes", "-t", &home, "-F", "#{pane_id}"]);
        assert!(home_panes.lines().any(|p| p == q), "queue is back in the home window");
    }
}
