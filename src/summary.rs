//! One digest of "what am I working on" for other apps (the TUI, cogitator, Waypoint).
use crate::ctx::Ctx;
use crate::tickets::{self, Ticket};
use crate::util::{git, iso_from_ms, ms_from_iso, now_ms, truncate};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MIN30: i64 = 30 * 60_000;

/// `git branch --show-current` is cached for 5 s: the summary is built every second while a client listens.
pub fn branch_of(session: &str, cwd: &str) -> Option<String> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = format!("{session}\0{cwd}");
    if let Some((t, v)) = cache.lock().ok()?.get(&key) {
        if t.elapsed() < Duration::from_secs(5) {
            return Some(v.clone()).filter(|b| !b.is_empty());
        }
    }
    let b = git(cwd, &["branch", "--show-current"]);
    cache.lock().ok()?.insert(key, (Instant::now(), b.clone()));
    Some(b).filter(|b| !b.is_empty())
}

/// Branch and the commits since the session started, for the state view. Cached like `branch_of`.
pub fn git_info(session: &str, cwd: &str, head_start: Option<&str>) -> Value {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((t, v)) = cache.lock().ok().and_then(|c| c.get(session).cloned()) {
        if t.elapsed() < Duration::from_secs(5) {
            return v;
        }
    }
    let commits: Vec<String> = match head_start.filter(|h| !h.is_empty()) {
        Some(h) => git(cwd, &["log", "--oneline", &format!("{h}..HEAD")]).lines().map(String::from).collect(),
        None => vec![],
    };
    let v = json!({"branch": git(cwd, &["branch", "--show-current"]), "commits": commits});
    if let Ok(mut c) = cache.lock() {
        c.insert(session.to_string(), (Instant::now(), v.clone()));
    }
    v
}

/// Live sessions (working, needs, turn), oldest state change first. A session whose tmux pane is gone is over.
pub fn live_sessions(ctx: &Ctx, conn: &Connection) -> Result<Vec<Value>> {
    let where_ = ctx.tmux.list_panes();
    let all = tickets::all(conn)?;
    let rows = crate::db::rows(
        conn,
        "SELECT * FROM sessions WHERE state IN ('needs','turn','working') ORDER BY state_at",
        [],
    )?;
    Ok(rows
        .into_iter()
        .filter(|s| s["tmux_pane"].as_str().is_none_or(|p| where_.contains_key(p)))
        .map(|s| {
            let sid = s["id"].as_str().unwrap_or("");
            let t = all.iter().find(|t| t.session.as_deref() == Some(sid) && t.status == "doing").or_else(|| all.iter().find(|t| t.session.as_deref() == Some(sid)));
            json!({
                "session": sid, "state": s["state"], "detail": s["state_detail"].as_str().unwrap_or(""), "since": s["state_at"],
                "repo": s["repo"], "branch": branch_of(sid, s["cwd"].as_str().unwrap_or("")),
                "tmux": s["tmux_pane"].as_str().and_then(|p| where_.get(p)),
                "key": t.map(|t| &t.key), "title": t.map(|t| &t.title), "project": t.and_then(|t| t.project.as_ref()),
                "message": truncate(s["last_message"].as_str().unwrap_or(""), 300), "cwd": s["cwd"], "transcript": s["transcript"],
            })
        })
        .collect())
}

pub struct PromptRow {
    pub at_ms: i64,
    /// Ticket key of the session, else its repo.
    pub thread: String,
}

/// Focus: threads prompted in the last 30 minutes, prompts in 30 minutes, the working streak in minutes
/// (no gap of 30 minutes or more), forgotten sessions (your turn for over 30 minutes) and orphaned `doing` tickets.
/// `prompts` is ordered by time.
pub fn focus(prompts: &[PromptRow], sessions: &[Value], doing: &[Ticket], now: i64) -> Value {
    let recent: Vec<&PromptRow> = prompts.iter().filter(|p| p.at_ms > now - MIN30).collect();
    let mut streak_start = None;
    for (i, p) in prompts.iter().enumerate() {
        if i == 0 || p.at_ms - prompts[i - 1].at_ms > MIN30 {
            streak_start = Some(p.at_ms);
        }
    }
    let active = prompts.last().is_some_and(|p| now - p.at_ms < MIN30);
    let streak = match (active, streak_start) {
        (true, Some(s)) => (now - s) as f64 / 60_000.0,
        _ => 0.0,
    };
    let mut threads: Vec<&str> = vec![];
    for p in &recent {
        if !threads.contains(&p.thread.as_str()) {
            threads.push(&p.thread);
        }
    }
    let forgotten: Vec<Value> = sessions
        .iter()
        .filter(|s| s["state"] == "turn")
        .filter_map(|s| {
            let waited = now - ms_from_iso(s["since"].as_str()?)?;
            (waited > MIN30).then(|| json!({"key": s["key"], "title": s["title"], "repo": s["repo"], "waiting_min": (waited as f64 / 60_000.0).round() as i64}))
        })
        .collect();
    let orphaned: Vec<Value> = doing
        .iter()
        .filter(|t| !sessions.iter().any(|s| t.session.as_deref().is_some_and(|id| s["session"] == id)))
        .map(|t| json!({"key": t.key, "title": t.title}))
        .collect();
    json!({"threads": threads, "prompts_30m": recent.len(), "streak_min": streak.round() as i64, "forgotten": forgotten, "orphaned": orphaned})
}

fn prompt_rows(conn: &Connection, now: i64) -> Result<Vec<PromptRow>> {
    let since = iso_from_ms(now - 12 * 3_600_000);
    let rows = crate::db::rows(
        conn,
        "SELECT p.at, s.repo, (SELECT key FROM tickets WHERE session=p.session LIMIT 1) key
         FROM prompts p JOIN sessions s ON s.id=p.session WHERE p.at > ? ORDER BY p.at",
        [since],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(PromptRow {
                at_ms: ms_from_iso(r["at"].as_str()?)?,
                thread: r["key"].as_str().or(r["repo"].as_str())?.to_string(),
            })
        })
        .collect())
}

pub fn summary(ctx: &Ctx) -> Result<Value> {
    summary_at(ctx, now_ms())
}

pub fn summary_at(ctx: &Ctx, now: i64) -> Result<Value> {
    let conn = ctx.db()?;
    let all = tickets::all(&conn)?;
    let sessions = live_sessions(ctx, &conn)?;
    let count = |st: &str| all.iter().filter(|t| t.status == st).count();
    let doing: Vec<Ticket> = all.iter().filter(|t| t.status == "doing").cloned().collect();
    // ponytail: "today" is the UTC date like the prototype; use the local date if the 1-2 h around midnight matter
    let today = &iso_from_ms(now)[..10];
    Ok(json!({
        "focus": focus(&prompt_rows(&conn, now)?, &sessions, &doing, now),
        "windows": crate::panes::windows(&ctx.tmux),
        "calls": ctx.calls.lock().map(|c| c.clone()).unwrap_or(Value::Null),
        "attention": sessions.iter().filter(|s| s["state"] != "working").collect::<Vec<_>>(),
        "working": sessions.iter().filter(|s| s["state"] == "working").collect::<Vec<_>>(),
        "done_today": all.iter().filter(|t| t.status == "done" && t.updated.starts_with(today))
            .map(|t| json!({"key": t.key, "title": t.title, "project": t.project, "at": t.updated})).collect::<Vec<_>>(),
        "ready": tickets::ready(&conn, None)?.iter()
            .map(|t| json!({"key": t.key, "title": t.title, "repo": t.repo, "project": t.project, "priority": t.priority})).collect::<Vec<_>>(),
        "tickets": {"backlog": count("backlog"), "todo": count("todo"), "doing": count("doing"), "done": count("done")},
        "projects": crate::db::rows(&conn, "SELECT p.name, p.palaver, COALESCE(sum(i.status='doing'),0) doing, COALESCE(sum(i.status IN ('todo','backlog')),0) open
            FROM projects p LEFT JOIN tickets i ON i.project=p.name GROUP BY p.name ORDER BY p.name", [])?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::Tmux;

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_800_000_000_000;

    fn p(min_ago: i64, thread: &str) -> PromptRow {
        PromptRow { at_ms: NOW - min_ago * MIN, thread: thread.into() }
    }

    fn ticket(key: &str, session: Option<&str>) -> Ticket {
        Ticket {
            key: key.into(),
            repo: "r".into(),
            title: format!("title {key}"),
            body: String::new(),
            status: "doing".into(),
            priority: 3,
            project: None,
            cluster: None,
            blocked_by: None,
            session: session.map(String::from),
            rank: 0.0,
            created: String::new(),
            updated: String::new(),
        }
    }

    #[test]
    fn threads_and_prompts_in_the_last_30_minutes() {
        let ps = [p(100, "OLD-1"), p(25, "A-1"), p(20, "r"), p(10, "A-1"), p(1, "B-2")];
        let f = focus(&ps, &[], &[], NOW);
        assert_eq!(f["threads"], json!(["A-1", "r", "B-2"]));
        assert_eq!(f["prompts_30m"], 4);
    }

    #[test]
    fn streak_runs_until_a_gap_of_30_minutes() {
        // prompts 95, 80, 50 (gap 30 -> not a break), 45, 5 minutes ago: gap of 40 before the last starts a new streak
        let ps = [p(95, "a"), p(80, "a"), p(50, "a"), p(45, "a"), p(5, "a")];
        assert_eq!(focus(&ps, &[], &[], NOW)["streak_min"], 5);
        // without the final gap the streak reaches back to the first prompt
        let ps = [p(95, "a"), p(70, "a"), p(45, "a"), p(20, "a"), p(5, "a")];
        assert_eq!(focus(&ps, &[], &[], NOW)["streak_min"], 95);
        // idle for 30+ minutes: no streak
        let ps = [p(95, "a"), p(40, "a")];
        assert_eq!(focus(&ps, &[], &[], NOW)["streak_min"], 0);
        assert_eq!(focus(&[], &[], &[], NOW)["streak_min"], 0);
    }

    #[test]
    fn forgotten_sessions_wait_more_than_30_minutes_for_you() {
        let since = |m: i64| iso_from_ms(NOW - m * MIN);
        let ss = vec![
            json!({"session": "s1", "state": "turn", "since": since(45), "key": "A-1", "title": "t", "repo": "r"}),
            json!({"session": "s2", "state": "turn", "since": since(10), "key": "A-2", "title": "t", "repo": "r"}),
            json!({"session": "s3", "state": "working", "since": since(90), "key": "A-3", "title": "t", "repo": "r"}),
            json!({"session": "s4", "state": "needs", "since": since(90), "key": "A-4", "title": "t", "repo": "r"}),
        ];
        let f = focus(&[], &ss, &[], NOW);
        assert_eq!(f["forgotten"].as_array().unwrap().len(), 1);
        assert_eq!(f["forgotten"][0]["key"], "A-1");
        assert_eq!(f["forgotten"][0]["waiting_min"], 45);
    }

    #[test]
    fn orphaned_are_doing_tickets_without_a_live_session() {
        let ss = vec![json!({"session": "live", "state": "working", "since": iso_from_ms(NOW)})];
        let doing = [ticket("A-1", Some("live")), ticket("A-2", Some("gone")), ticket("A-3", None)];
        let f = focus(&[], &ss, &doing, NOW);
        let keys: Vec<_> = f["orphaned"].as_array().unwrap().iter().map(|o| o["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["A-2", "A-3"]);
    }

    #[test]
    fn summary_groups_attention_working_ready_and_done_today() {
        let d = crate::db::tempdir::Dir::new();
        let ctx = Ctx::for_test(d.path(), Tmux::private("summary-unit"));
        let c = ctx.db().unwrap();
        let mk = |title: &str, project: Option<&str>| {
            tickets::create(
                &c,
                tickets::NewTicket {
                    repo: "r".into(),
                    title: title.into(),
                    project: project.map(String::from),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let a = mk("ready one", Some("P"));
        let done = mk("finished", Some("P"));
        let busy = mk("busy", None);
        let set = |k: &str, st: &str| tickets::update(&c, k, json!({"status": st}).as_object().unwrap(), None).unwrap();
        set(&done, "done");
        set(&busy, "doing");
        // sessions without a tmux pane count as live
        for (id, state, mins) in [("w", "working", 5), ("t", "turn", 50), ("n", "needs", 20)] {
            c.execute(
                "INSERT INTO sessions(id,repo,cwd,state,state_at) VALUES(?,?,?,?,?)",
                rusqlite::params![id, "r", "/nonexistent", state, iso_from_ms(NOW - mins * MIN)],
            )
            .unwrap();
        }
        c.execute("UPDATE tickets SET session='w' WHERE key=?", [&busy]).unwrap();
        c.execute("UPDATE tickets SET updated=? WHERE key=?", [iso_from_ms(NOW - MIN), done.clone()]).unwrap();
        let s = summary_at(&ctx, NOW).unwrap();
        let keys = |v: &Value| {
            v.as_array().unwrap().iter().map(|x| x["session"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(keys(&s["attention"]), ["t", "n"], "oldest state change first");
        assert_eq!(keys(&s["working"]), ["w"]);
        assert_eq!(s["working"][0]["key"], busy);
        assert_eq!(s["ready"][0]["key"], a);
        assert_eq!(s["done_today"][0]["key"], done);
        assert_eq!(s["tickets"], json!({"backlog": 0, "todo": 1, "doing": 1, "done": 1}));
        assert_eq!(s["projects"][0], json!({"name": "P", "palaver": null, "doing": 0, "open": 1}));
        assert_eq!(s["focus"]["forgotten"][0]["waiting_min"], 50);
        assert!(s["calls"].is_object());
        assert_eq!(s["windows"], json!([]), "no cockpit session, no windows");
    }
}
