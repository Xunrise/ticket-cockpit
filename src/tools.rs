//! The tool registry: every cockpit capability as a named tool with a description and params.
//! One table serves HTTP (`/api/tools`), the CLI (`cockpit tool NAME '{json}'`) and, later, MCP.
use crate::ctx::Ctx;
use crate::panes::{self, Sel};
use crate::projects::{self, Service};
use crate::tickets::{self, NewTicket, Ticket};
use crate::{connections, palaver, settings, summary};
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

pub struct Tool {
    pub name: &'static str,
    pub desc: &'static str,
    pub params: &'static [(&'static str, &'static str)],
    pub run: fn(&Ctx, Value) -> Result<Value>,
}

const PANE_KEY: (&str, &str) = ("key", "ticket key (or caller_pane)");

pub const TOOLS: &[Tool] = &[
    Tool { name: "settings.get", desc: "All cockpit settings with their values, defaults and descriptions", params: &[], run: |c, _| settings::all(&c.db()?).map(Value::Array) },
    Tool {
        name: "settings.set",
        desc: "Change a cockpit setting (null resets it to the default)",
        params: &[("key", "setting key"), ("value", "new value (same type as the default)")],
        run: |c, a| {
            let key = str_arg(&a, "key")?;
            let value = settings::set(&c.db()?, &key, &a["value"])?;
            Ok(json!({"key": key, "value": value}))
        },
    },
    Tool { name: "connections.status", desc: "Status of the tools the cockpit talks to", params: &[], run: |c, _| Ok(connections::status(c)) },
    Tool { name: "cockpit.summary", desc: "What needs attention, what is being worked on, ready tickets, projects, focus and calls", params: &[], run: |c, _| summary::summary(c) },
    Tool { name: "project.list", desc: "Cockpit projects and their Palaver link", params: &[], run: |c, _| projects::list(&c.db()?).map(Value::Array) },
    Tool {
        name: "project.overview",
        desc: "A project: planned tickets by cluster, in progress, recently done, live sessions",
        params: &[("project", "project name")],
        run: |c, a| {
            let conn = c.db()?;
            projects::overview(&conn, &str_arg(&a, "project")?, &summary::live_sessions(c, &conn)?)
        },
    },
    Tool {
        name: "project.link_palaver",
        desc: "Link a cockpit project to a Palaver project",
        params: &[("project", "cockpit project"), ("palaver", "Palaver project name")],
        run: |c, a| {
            let (project, pv) = (str_arg(&a, "project")?, str_arg(&a, "palaver")?);
            projects::link_palaver(&c.db()?, &project, &pv)?;
            Ok(json!({"project": project, "palaver": pv}))
        },
    },
    Tool {
        name: "project.set_services",
        desc: "Set a project's default panes, e.g. [{name:'backend', cmd:'npm run api', cwd:'server'}]",
        params: &[("project", "project name"), ("services", "list of {name, cmd, cwd?}")],
        run: |c, a| {
            let project = str_arg(&a, "project")?;
            let services: Vec<Service> = serde_json::from_value(a["services"].clone()).map_err(|e| anyhow!("services must be a list of {{name, cmd, cwd?}}: {e}"))?;
            projects::set_services(&c.db()?, &project, &services)?;
            Ok(json!({"project": project, "services": services}))
        },
    },
    Tool {
        name: "project.housekeep",
        desc: "Start a Claude session that tidies a project's planned tickets: duplicates, clusters, titles, stale items",
        params: &[("project", "project name")],
        run: |c, a| {
            let conn = c.db()?;
            let project = str_arg(&a, "project")?;
            let repo = projects::main_repo(&conn, &project)?.ok_or_else(|| anyhow!("project {project} has no tickets"))?;
            let tk = format!("{} tool", c.exe);
            let prompt = format!(
                "Housekeeping for the cockpit project \"{project}\". Do not implement anything. Read the project with: {tk} project.overview '{{\"project\":\"{project}\"}}'. Then tidy its planned (todo/backlog) tickets: merge duplicates (cancel the duplicate with {tk} ticket.update '{{\"key\":\"…\",\"status\":\"canceled\"}}' and fold its details into the kept one), group them into a few clear clusters ({tk} ticket.update '{{\"key\":\"…\",\"cluster\":\"…\"}}'), sharpen vague titles, and set priority 1-4 where obvious. Use the done tickets as context for what is already built. Finish with a short list of what you changed and any ticket you think should be dropped (ask before dropping)."
            );
            let name = format!("{}-housekeeping", project.chars().filter(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '-').collect::<String>());
            Ok(json!({"window": panes::run_claude(&conn, &c.tmux, &repo, &name, &prompt)?}))
        },
    },
    Tool { name: "ticket.get", desc: "A ticket with its links and sessions", params: &[("key", "ticket key")], run: |c, a| ticket_view(c, &c.db()?, &tickets::get(&c.db()?, &str_arg(&a, "key")?)?) },
    Tool {
        name: "ticket.add",
        desc: "Add a planned ticket to a project (for later or parallel work)",
        params: &[
            ("project", "project name"),
            ("title", "short title"),
            ("body", "optional details"),
            ("cluster", "optional cluster"),
            ("repo", "optional repo; defaults to the caller session repo, the caller pane's session repo or the project main repo"),
            ("session", "optional calling session id"),
        ],
        run: |c, a| {
            let conn = c.db()?;
            let project = str_arg(&a, "project")?;
            if !projects::exists(&conn, &project)? {
                bail!("no project {project}");
            }
            let session = opt_str(&a, "session");
            let repo = opt_str(&a, "repo")
                .or_else(|| session.as_deref().and_then(|s| conn.query_row("SELECT repo FROM sessions WHERE id=?", [s], |r| r.get(0)).optional().ok().flatten()))
                .or_else(|| {
                    let pane = opt_str(&a, "caller_pane")?;
                    let pane = format!("cockpit:{}", pane.trim_start_matches("cockpit:"));
                    conn.query_row("SELECT repo FROM sessions WHERE tmux_pane=? ORDER BY started DESC", [pane], |r| r.get(0)).optional().ok().flatten()
                })
                .or_else(|| projects::main_repo(&conn, &project).ok().flatten())
                .ok_or_else(|| anyhow!("no repo: pass \"repo\""))?;
            let title = str_arg(&a, "title")?;
            let key = tickets::create(
                &conn,
                NewTicket { repo, title: title.clone(), body: opt_str(&a, "body").unwrap_or_default(), project: Some(project.clone()), cluster: opt_str(&a, "cluster"), ..Default::default() },
            )?;
            if let Some(s) = &session {
                tickets::event(&conn, &key, Some(s), "comment", "added from a Claude session")?;
            }
            Ok(json!({"key": key, "project": project, "title": title}))
        },
    },
    Tool {
        name: "ticket.update",
        desc: "Change a ticket: title, body, status (backlog|todo|doing|done|canceled), priority, cluster, project",
        params: &[("key", "ticket key"), ("...", "fields to change")],
        run: |c, a| {
            let mut fields = a.as_object().cloned().unwrap_or_default();
            let key = fields.remove("key").and_then(|k| k.as_str().map(String::from)).ok_or_else(|| anyhow!("key required"))?;
            fields.remove("caller_pane");
            Ok(serde_json::to_value(tickets::update(&c.db()?, &key, &fields, None)?)?)
        },
    },
    Tool {
        name: "ticket.set_project",
        desc: "Assign a ticket to a cockpit project (created if new)",
        params: &[("key", "ticket key"), ("project", "project name")],
        run: |c, a| {
            let (key, project) = (str_arg(&a, "key")?, str_arg(&a, "project")?);
            let fields = json!({"project": project}).as_object().cloned().unwrap_or_default();
            tickets::update(&c.db()?, &key, &fields, None)?;
            Ok(json!({"key": key, "project": project}))
        },
    },
    Tool { name: "ticket.links", desc: "Context linked to a ticket", params: &[("key", "ticket key")], run: |c, a| tickets::links(&c.db()?, &str_arg(&a, "key")?).map(Value::Array) },
    Tool {
        name: "ticket.start",
        desc: "Start a Claude session for a ticket in its own window of the cockpit's tmux",
        params: &[("key", "ticket key")],
        run: |c, a| Ok(json!({"window": panes::start_ticket(&c.db()?, &c.tmux, &str_arg(&a, "key")?)?})),
    },
    Tool {
        name: "ticket.link_meeting",
        desc: "Link a Palaver meeting to a ticket: saves notes and transcript as a file, and pastes a reference into the ticket's live session prompt (not submitted)",
        params: &[("key", "ticket key"), ("meeting_id", "Palaver meeting id"), ("why", "optional: why it is relevant")],
        run: |c, a| {
            palaver::link_meeting(&c.db()?, &c.tmux, &c.palaver_bin, &c.context_dir, &str_arg(&a, "key")?, &str_arg(&a, "meeting_id")?, &opt_str(&a, "why").unwrap_or_default())
        },
    },
    Tool {
        name: "ticket.auto_link_meetings",
        desc: "Start a Claude session that reads the ticket's Palaver meetings and links the relevant ones",
        params: &[("key", "ticket key")],
        run: |c, a| palaver::auto_link(&c.db()?, &c.tmux, &c.exe, &str_arg(&a, "key")?),
    },
    Tool {
        name: "ticket.for_change",
        desc: "Find the ticket behind a change: by session id, else repo path + branch, else a ticket key in the branch name",
        params: &[("session", "optional Claude session id"), ("repo_path", "optional repo toplevel path"), ("branch", "optional branch name")],
        run: |c, a| for_change(c, &c.db()?, opt_str(&a, "session"), opt_str(&a, "repo_path"), opt_str(&a, "branch")),
    },
    Tool {
        name: "ticket.link_case",
        desc: "Link a Waypoint case to a ticket",
        params: &[("key", "ticket key"), ("case_id", "Waypoint case id"), ("title", "optional case title"), ("url", "optional case URL")],
        run: |c, a| {
            let (key, case_id) = (str_arg(&a, "key")?, str_arg(&a, "case_id")?);
            tickets::link_case(&c.db()?, &key, &case_id, &opt_str(&a, "title").unwrap_or_else(|| case_id.clone()), opt_str(&a, "url").as_deref())?;
            Ok(json!({"key": key, "case_id": case_id}))
        },
    },
    Tool {
        name: "session.get",
        desc: "A Claude session: repo, toplevel path, cwd, branch, state, tmux window, ticket",
        params: &[("session", "Claude session id")],
        run: |c, a| {
            let id = str_arg(&a, "session")?;
            let conn = c.db()?;
            let s = crate::db::rows(&conn, "SELECT * FROM sessions WHERE id=?", [&id])?.into_iter().next().ok_or_else(|| anyhow!("no session {id}"))?;
            Ok(session_view(c, &conn, &s, &c.tmux.list_panes()))
        },
    },
    Tool { name: "palaver.projects", desc: "Palaver projects", params: &[], run: |c, _| palaver::projects(&c.palaver_bin).map(Value::Array) },
    Tool {
        name: "palaver.meetings",
        desc: "Meetings of a ticket's Palaver project, newest first",
        params: &[("key", "ticket key"), ("limit", "optional, default 50")],
        run: |c, a| palaver::meetings(&c.db()?, &c.palaver_bin, &str_arg(&a, "key")?, a["limit"].as_i64().unwrap_or(50)).map(Value::Array),
    },
    Tool {
        name: "palaver.coach",
        desc: "The latest call and Palaver's speaking-coach baseline",
        params: &[],
        run: |c, _| {
            let fresh = palaver::calls(&c.palaver_bin, &c.coach_file);
            if let Ok(mut g) = c.calls.lock() {
                *g = fresh.clone();
            }
            Ok(fresh)
        },
    },
    Tool { name: "pane.list", desc: "Panes in a ticket's cockpit window", params: &[PANE_KEY], run: |c, a| Ok(serde_json::to_value(panes::list(&c.tmux, &sel(&a))?)?) },
    Tool {
        name: "pane.run",
        desc: "Run a long-running command (server, watcher) in a named pane of the ticket window; restarts it if the name exists",
        params: &[PANE_KEY, ("name", "pane name, e.g. backend"), ("cmd", "shell command"), ("cwd", "optional working directory")],
        run: |c, a| panes::run(&c.tmux, &sel(&a), &str_arg(&a, "name")?, &str_arg(&a, "cmd")?, opt_str(&a, "cwd").as_deref()),
    },
    Tool {
        name: "pane.layout",
        desc: "Re-arrange a ticket window: Claude on top, services in a strip below",
        params: &[PANE_KEY],
        run: |c, a| {
            let s = sel(&a);
            panes::layout(&c.tmux, &panes::window_id(&c.tmux, &s)?)?;
            Ok(serde_json::to_value(panes::list(&c.tmux, &s)?)?)
        },
    },
    Tool {
        name: "pane.logs",
        desc: "Last lines of a named pane",
        params: &[PANE_KEY, ("name", "pane name"), ("lines", "optional, default 40")],
        run: |c, a| Ok(Value::String(panes::logs(&c.tmux, &sel(&a), &str_arg(&a, "name")?, a["lines"].as_u64().unwrap_or(40) as u32)?)),
    },
    Tool { name: "pane.stop", desc: "Stop and close a named pane", params: &[PANE_KEY, ("name", "pane name")], run: |c, a| panes::stop(&c.tmux, &sel(&a), &str_arg(&a, "name")?) },
    Tool {
        name: "pane.up",
        desc: "Start the ticket project's default panes (project.set_services)",
        params: &[("key", "ticket key (or caller_pane of a session with a ticket)")],
        run: |c, a| {
            let conn = c.db()?;
            let mut s = sel(&a);
            if s.key.is_none() {
                s.key = s.caller_pane.as_deref().and_then(|p| ticket_of_pane(&conn, p));
            }
            panes::up(&conn, &c.tmux, &s)
        },
    },
];

fn str_arg(a: &Value, k: &str) -> Result<String> {
    opt_str(a, k).ok_or_else(|| anyhow!("{k} required"))
}

fn opt_str(a: &Value, k: &str) -> Option<String> {
    a[k].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// An explicit ticket key wins over the caller's own pane.
fn sel(a: &Value) -> Sel {
    let key = opt_str(a, "key");
    Sel { caller_pane: if key.is_some() { None } else { opt_str(a, "caller_pane") }, key }
}

fn ticket_of_pane(conn: &Connection, pane: &str) -> Option<String> {
    let pane = format!("cockpit:{}", pane.trim_start_matches("cockpit:"));
    let sid: String = conn
        .query_row("SELECT id FROM sessions WHERE tmux_pane=? ORDER BY started DESC", [pane], |r| r.get(0))
        .optional()
        .ok()??;
    tickets::ticket_of_session(conn, &sid).ok()?.map(|t| t.key)
}

fn session_view(c: &Ctx, conn: &Connection, s: &Value, where_: &std::collections::BTreeMap<String, String>) -> Value {
    let sid = s["id"].as_str().unwrap_or("");
    let repo = s["repo"].as_str().unwrap_or("");
    json!({
        "session": sid, "repo": repo, "repo_path": tickets::repo_path(conn, repo).ok().flatten(), "cwd": s["cwd"],
        "branch": summary::branch_of(sid, s["cwd"].as_str().unwrap_or("")),
        "state": if s["ended"].is_string() { json!("ended") } else { s["state"].clone() }, "since": s["state_at"],
        "tmux": s["tmux_pane"].as_str().and_then(|p| where_.get(p)),
        "key": tickets::ticket_of_session(conn, sid).ok().flatten().map(|t| t.key),
        "pane": s["tmux_pane"], "alive": s["tmux_pane"].as_str().is_some_and(|p| where_.contains_key(p)) && c.db_path.exists(),
    })
}

fn ticket_view(c: &Ctx, conn: &Connection, t: &Ticket) -> Result<Value> {
    let where_ = c.tmux.list_panes();
    let sessions: Vec<Value> = crate::db::rows(conn, "SELECT * FROM sessions WHERE id IN (SELECT session FROM events WHERE ticket=? AND session IS NOT NULL) OR id=?", params![t.key, t.session])?
        .iter()
        .map(|s| session_view(c, conn, s, &where_))
        .collect();
    let mut v = serde_json::to_value(t)?;
    v["links"] = Value::Array(tickets::links(conn, &t.key)?);
    v["sessions"] = Value::Array(sessions);
    Ok(v)
}

/// First `[a-z]{2,5}-\d+` in a branch name, upper-cased: `feature/dem-7-x` gives `DEM-7`.
pub fn key_in_branch(branch: &str) -> Option<String> {
    let b: Vec<char> = branch.chars().collect();
    for i in 0..b.len() {
        for len in (2..=5).rev() {
            let end = i + len;
            if end < b.len()
                && b[i..end].iter().all(|c| c.is_ascii_alphabetic())
                && b[end] == '-'
                && b.get(end + 1).is_some_and(|c| c.is_ascii_digit())
            {
                let digits: String = b[end + 1..].iter().take_while(|c| c.is_ascii_digit()).collect();
                return Some(format!("{}-{digits}", b[i..end].iter().collect::<String>().to_uppercase()));
            }
        }
    }
    None
}

/// Join order for Waypoint: the Claude session id, then repo toplevel + branch, then a ticket key in the branch name.
fn for_change(
    c: &Ctx,
    conn: &Connection,
    session: Option<String>,
    repo_path: Option<String>,
    branch: Option<String>,
) -> Result<Value> {
    let found = |m: &str, t: &Ticket| -> Result<Value> { Ok(json!({"match": m, "ticket": ticket_view(c, conn, t)?})) };
    if let Some(t) = session.as_deref().and_then(|s| tickets::ticket_of_session(conn, s).ok().flatten()) {
        return found("session", &t);
    }
    if let (Some(path), Some(branch)) = (&repo_path, &branch) {
        let repo: Option<String> =
            conn.query_row("SELECT name FROM repos WHERE path=?", [path], |r| r.get(0)).optional()?;
        if let Some(repo) = repo {
            let mut st = conn.prepare("SELECT id, cwd FROM sessions WHERE repo=? ORDER BY started DESC")?;
            let rows = st
                .query_map([repo], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (id, cwd) in rows {
                if summary::branch_of(&id, cwd.as_deref().unwrap_or("")).as_deref() == Some(branch) {
                    if let Some(t) = tickets::ticket_of_session(conn, &id)? {
                        return found("branch", &t);
                    }
                }
            }
        }
    }
    if let Some(t) = branch.as_deref().and_then(key_in_branch).and_then(|k| tickets::find(conn, &k).ok().flatten()) {
        return found("key-in-branch", &t);
    }
    Ok(json!({"match": null, "ticket": null}))
}

pub fn call(ctx: &Ctx, name: &str, args: Value) -> Result<Value> {
    let tool = TOOLS.iter().find(|t| t.name == name).ok_or_else(|| anyhow!("no tool {name}"))?;
    (tool.run)(ctx, if args.is_null() { json!({}) } else { args })
}

pub fn describe() -> Value {
    Value::Array(
        TOOLS
            .iter()
            .map(|t| json!({"name": t.name, "desc": t.desc, "params": t.params.iter().map(|(k, d)| (k.to_string(), json!(d))).collect::<Map<String, Value>>()}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::tmux::Tmux;

    fn ctx() -> (Dir, Ctx) {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("tools-unit"));
        (d, c)
    }

    fn t(c: &Ctx, name: &str, args: Value) -> Value {
        call(c, name, args).unwrap_or_else(|e| panic!("{name}: {e:#}"))
    }

    fn mk(c: &Ctx, repo: &str, project: Option<&str>) -> String {
        let conn = c.db().unwrap();
        tickets::create(
            &conn,
            NewTicket {
                repo: repo.into(),
                title: format!("t-{repo}"),
                project: project.map(String::from),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn registry_has_every_documented_tool() {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        for n in [
            "settings.get",
            "settings.set",
            "connections.status",
            "project.list",
            "project.overview",
            "project.link_palaver",
            "project.set_services",
            "project.housekeep",
            "ticket.get",
            "ticket.add",
            "ticket.update",
            "ticket.set_project",
            "ticket.links",
            "ticket.link_meeting",
            "ticket.auto_link_meetings",
            "ticket.for_change",
            "ticket.link_case",
            "session.get",
            "palaver.projects",
            "palaver.meetings",
            "palaver.coach",
            "pane.list",
            "pane.run",
            "pane.logs",
            "pane.stop",
            "pane.up",
            "pane.layout",
            "cockpit.summary",
        ] {
            assert!(names.contains(&n), "{n}");
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicate names");
        let d = describe();
        assert!(d.as_array().unwrap().iter().all(|t| t["desc"].as_str().is_some_and(|s| !s.is_empty())));
        assert!(call(&ctx().1, "nope", json!({})).is_err());
    }

    #[test]
    fn branch_keys_are_found_case_insensitively() {
        assert_eq!(key_in_branch("feature/dem-7-login").as_deref(), Some("DEM-7"));
        assert_eq!(key_in_branch("DEM-12").as_deref(), Some("DEM-12"));
        assert_eq!(key_in_branch("fix-nothing"), None);
        assert_eq!(key_in_branch("x-1"), None, "prefix needs two letters");
        assert_eq!(key_in_branch("bugfix/WISH-3").as_deref(), Some("WISH-3"));
    }

    #[test]
    fn for_change_joins_session_then_branch_then_key() {
        let (d, c) = ctx();
        let conn = c.db().unwrap();
        let by_session = mk(&c, "demo", None);
        let by_branch = mk(&c, "demo", None);
        let by_key = mk(&c, "demo", None);
        // a real git repo so the session's branch can be read
        let repo = d.path().join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |a: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(a)
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success())
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["commit", "-q", "--allow-empty", "-m", "x"]);
        g(&["checkout", "-q", "-b", "work/thing"]);
        let repo_s = repo.to_string_lossy().to_string();
        conn.execute("INSERT OR REPLACE INTO repos VALUES('demo', ?)", [&repo_s]).unwrap();
        for (sid, key) in [("sess-a", &by_session), ("sess-b", &by_branch)] {
            conn.execute(
                "INSERT INTO sessions(id,repo,cwd,started) VALUES(?,?,?,?)",
                params![sid, "demo", repo_s, if sid == "sess-a" { "2026-01-01" } else { "2026-01-02" }],
            )
            .unwrap();
            conn.execute("UPDATE tickets SET session=?, status='doing' WHERE key=?", params![sid, key]).unwrap();
        }
        // 1. the session id wins, even when the branch points elsewhere
        let r = t(&c, "ticket.for_change", json!({"session": "sess-a", "repo_path": repo_s, "branch": "work/thing"}));
        assert_eq!((r["match"].as_str(), r["ticket"]["key"].as_str()), (Some("session"), Some(by_session.as_str())));
        // 2. repo path + branch: the newest session of the repo on that branch
        let r = t(&c, "ticket.for_change", json!({"repo_path": repo_s, "branch": "work/thing"}));
        assert_eq!((r["match"].as_str(), r["ticket"]["key"].as_str()), (Some("branch"), Some(by_branch.as_str())));
        // 3. a ticket key in the branch name
        let branch = format!("feat/{}-x", by_key.to_lowercase());
        let r = t(&c, "ticket.for_change", json!({"repo_path": "/elsewhere", "branch": branch}));
        assert_eq!((r["match"].as_str(), r["ticket"]["key"].as_str()), (Some("key-in-branch"), Some(by_key.as_str())));
        assert!(r["ticket"]["links"].is_array() && r["ticket"]["sessions"].is_array());
        // nothing matches
        let r = t(&c, "ticket.for_change", json!({"branch": "main"}));
        assert!(r["match"].is_null() && r["ticket"].is_null());
    }

    #[test]
    fn ticket_add_repo_fallback_chain() {
        let (_d, c) = ctx();
        let conn = c.db().unwrap();
        for _ in 0..4 {
            mk(&c, "main-repo", Some("P"));
        }
        mk(&c, "other", Some("P"));
        conn.execute("INSERT INTO sessions(id,repo,cwd,started,tmux_pane) VALUES('s9','session-repo','/x','2026-01-01','cockpit:%5')", []).unwrap();
        let repo_of = |k: &Value| tickets::get(&conn, k["key"].as_str().unwrap()).unwrap().repo;
        // explicit repo > session > caller pane > project main repo
        assert_eq!(
            repo_of(&t(
                &c,
                "ticket.add",
                json!({"project": "P", "title": "a", "repo": "explicit", "session": "s9", "caller_pane": "%5"})
            )),
            "explicit"
        );
        assert_eq!(
            repo_of(&t(&c, "ticket.add", json!({"project": "P", "title": "b", "session": "s9", "caller_pane": "%5"}))),
            "session-repo"
        );
        assert_eq!(
            repo_of(&t(&c, "ticket.add", json!({"project": "P", "title": "c", "caller_pane": "%5"}))),
            "session-repo"
        );
        assert_eq!(repo_of(&t(&c, "ticket.add", json!({"project": "P", "title": "d"}))), "main-repo");
        assert!(call(&c, "ticket.add", json!({"project": "missing", "title": "x"})).is_err());
        let r = t(&c, "ticket.add", json!({"project": "P", "title": "e", "cluster": "ui", "session": "s9"}));
        let tk = tickets::get(&conn, r["key"].as_str().unwrap()).unwrap();
        assert_eq!(tk.cluster.as_deref(), Some("ui"));
        assert!(tickets::events(&conn, &tk.key).unwrap().iter().any(|e| e["text"] == "added from a Claude session"));
    }

    #[test]
    fn overview_tool_clusters_and_includes_live_sessions() {
        let (_d, c) = ctx();
        let a = mk(&c, "r", Some("P"));
        let b = mk(&c, "r", Some("P"));
        t(&c, "ticket.update", json!({"key": a, "cluster": "zeta"}));
        let o = t(&c, "project.overview", json!({"project": "P"}));
        let keys: Vec<&str> = o["todo"].as_array().unwrap().iter().map(|x| x["key"].as_str().unwrap()).collect();
        assert_eq!(keys, [a.as_str(), b.as_str()], "clustered first, unsorted last");
        assert_eq!(o["clusters"], json!(["zeta"]));
        assert!(call(&c, "project.overview", json!({"project": "none"})).is_err());
    }

    #[test]
    fn settings_projects_and_links_through_tools() {
        let (_d, c) = ctx();
        assert_eq!(t(&c, "settings.get", json!({})).as_array().unwrap().len(), 6);
        t(&c, "settings.set", json!({"key": "queue_width", "value": 45}));
        let s = t(&c, "settings.get", json!({}));
        assert_eq!(s.as_array().unwrap().iter().find(|x| x["key"] == "queue_width").unwrap()["value"], 45);
        assert!(call(&c, "settings.set", json!({"key": "queue_width", "value": "wide"})).is_err());
        t(&c, "settings.set", json!({"key": "queue_width", "value": null}));
        let k = mk(&c, "r", None);
        t(&c, "ticket.set_project", json!({"key": k, "project": "New"}));
        t(&c, "project.link_palaver", json!({"project": "New", "palaver": "PV"}));
        t(&c, "project.set_services", json!({"project": "New", "services": [{"name": "a", "cmd": "b"}]}));
        assert_eq!(t(&c, "project.list", json!({}))[0]["palaver"], "PV");
        t(&c, "ticket.link_case", json!({"key": k, "case_id": "case-1", "url": "http://x"}));
        let tk = t(&c, "ticket.get", json!({"key": k}));
        assert_eq!(tk["links"][0]["kind"], "waypoint");
        assert_eq!(tk["project"], "New");
        assert!(call(&c, "project.set_services", json!({"project": "New", "services": "no"})).is_err());
    }

    #[test]
    fn caller_pane_is_dropped_when_a_key_is_given() {
        let s = sel(&json!({"key": "A-1", "caller_pane": "%3"}));
        assert_eq!((s.key.as_deref(), s.caller_pane), (Some("A-1"), None));
        let s = sel(&json!({"caller_pane": "%3"}));
        assert_eq!((s.key, s.caller_pane.as_deref()), (None, Some("%3")));
    }
}
