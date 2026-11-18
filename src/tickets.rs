//! Tickets, their event log and context links.
use crate::util::{git, now_iso};
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::Path;

pub const STATUSES: [&str; 5] = ["backlog", "todo", "doing", "done", "canceled"];
const EDITABLE: [&str; 8] = ["title", "body", "status", "priority", "project", "blocked_by", "rank", "cluster"];

#[derive(Debug, Clone, Serialize)]
pub struct Ticket {
    pub key: String,
    pub repo: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: i64,
    pub project: Option<String>,
    pub cluster: Option<String>,
    pub blocked_by: Option<String>,
    pub session: Option<String>,
    pub rank: f64,
    pub created: String,
    pub updated: String,
}

const COLS: &str = "key,repo,title,body,status,priority,project,cluster,blocked_by,session,rank,created,updated";

impl Ticket {
    pub fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Ticket {
            key: r.get(0)?,
            repo: r.get(1)?,
            title: r.get(2)?,
            body: r.get(3)?,
            status: r.get(4)?,
            priority: r.get(5)?,
            project: r.get(6)?,
            cluster: r.get(7)?,
            blocked_by: r.get(8)?,
            session: r.get(9)?,
            rank: r.get(10)?,
            created: r.get(11)?,
            updated: r.get(12)?,
        })
    }
}

#[derive(Debug, Default)]
pub struct NewTicket {
    pub repo: String,
    pub title: String,
    pub body: String,
    pub project: Option<String>,
    pub cluster: Option<String>,
    pub blocked_by: Option<String>,
    pub priority: Option<i64>,
    pub status: Option<String>,
}

/// The main checkout a path belongs to (worktrees resolve to their main repo); None outside a git repo.
pub fn main_checkout(cwd: &str) -> Option<String> {
    let common = git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    Path::new(&common).parent().filter(|_| !common.is_empty()).map(|p| p.to_string_lossy().into_owned())
}

/// The main checkout a path belongs to (a non-git path stands for itself); remembered in `repos`.
pub fn repo_at(conn: &Connection, cwd: &str) -> Result<String> {
    let path = main_checkout(cwd).unwrap_or_else(|| cwd.to_string());
    remember_repo(conn, &path)
}

/// Stores `path` under its directory name in `repos` and returns that name.
pub fn remember_repo(conn: &Connection, path: &str) -> Result<String> {
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    conn.execute(
        "INSERT INTO repos(name,path) VALUES(?,?) ON CONFLICT(name) DO UPDATE SET path=excluded.path",
        params![name, path],
    )?;
    Ok(name)
}

pub fn repo_path(conn: &Connection, repo: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT path FROM repos WHERE name=?", [repo], |r| r.get(0)).optional()?)
}

pub fn event(conn: &Connection, ticket: &str, session: Option<&str>, kind: &str, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO events(ticket,session,kind,text,at) VALUES(?,?,?,?,?)",
        params![ticket, session, kind, text, now_iso()],
    )?;
    Ok(())
}

pub fn events(conn: &Connection, key: &str) -> Result<Vec<Value>> {
    crate::db::rows(conn, "SELECT * FROM events WHERE ticket=? ORDER BY id", [key])
}

pub fn find(conn: &Connection, key: &str) -> Result<Option<Ticket>> {
    Ok(conn.query_row(&format!("SELECT {COLS} FROM tickets WHERE key=?"), [key], Ticket::from_row).optional()?)
}

pub fn get(conn: &Connection, key: &str) -> Result<Ticket> {
    find(conn, key)?.ok_or_else(|| anyhow!("no ticket {key}"))
}

pub fn all(conn: &Connection) -> Result<Vec<Ticket>> {
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM tickets ORDER BY rank"))?;
    let v = st.query_map([], Ticket::from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(v)
}

pub fn ensure_project(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO projects(name) VALUES(?)", [name])?;
    Ok(())
}

fn prefix(repo: &str) -> String {
    let p: String = repo.chars().filter(|c| c.is_ascii_alphabetic()).take(3).collect::<String>().to_uppercase();
    if p.is_empty() {
        "TKT".into()
    } else {
        p
    }
}

fn check_status(s: &str) -> Result<()> {
    if STATUSES.contains(&s) {
        Ok(())
    } else {
        bail!("status must be one of {}", STATUSES.join("|"))
    }
}

fn check_priority(p: i64) -> Result<()> {
    if (1..=4).contains(&p) {
        Ok(())
    } else {
        bail!("priority must be 1-4")
    }
}

/// Key = first three letters of the repo + running number. The number continues across repos
/// sharing a prefix, so keys stay unique.
pub fn create(conn: &Connection, t: NewTicket) -> Result<String> {
    if t.title.trim().is_empty() {
        bail!("title required");
    }
    let status = t.status.unwrap_or_else(|| "todo".into());
    check_status(&status)?;
    let priority = t.priority.unwrap_or(3);
    check_priority(priority)?;
    let pre = prefix(&t.repo);
    let n: i64 = conn.query_row(
        "SELECT COALESCE(MAX(CAST(substr(key, ?) AS INTEGER)), 0) FROM tickets WHERE key LIKE ?",
        params![pre.len() as i64 + 2, format!("{pre}-%")],
        |r| r.get(0),
    )?;
    let key = format!("{pre}-{}", n + 1);
    let rank: f64 = conn.query_row("SELECT COALESCE(MAX(rank),0)+1 FROM tickets", [], |r| r.get(0))?;
    let project = t.project.filter(|p| !p.is_empty());
    if let Some(p) = &project {
        ensure_project(conn, p)?;
    }
    let now = now_iso();
    conn.execute(
        "INSERT INTO tickets(key,repo,title,body,status,priority,project,cluster,blocked_by,rank,created,updated) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
        params![key, t.repo, t.title, t.body, status, priority, project, t.cluster.filter(|c| !c.is_empty()), t.blocked_by.filter(|b| !b.is_empty()), rank, now, now],
    )?;
    event(conn, &key, None, "created", "")?;
    Ok(key)
}

/// Applies editable fields; empty strings clear. Status changes are logged.
pub fn update(conn: &Connection, key: &str, fields: &Map<String, Value>, session: Option<&str>) -> Result<Ticket> {
    let before = get(conn, key)?;
    for (k, v) in fields {
        if !EDITABLE.contains(&k.as_str()) {
            continue;
        }
        let sql_val: rusqlite::types::Value = match (k.as_str(), v) {
            ("priority", v) => {
                let p = v.as_i64().ok_or_else(|| anyhow!("priority must be a number"))?;
                check_priority(p)?;
                p.into()
            }
            ("rank", v) => v.as_f64().ok_or_else(|| anyhow!("rank must be a number"))?.into(),
            (_, Value::Null) => rusqlite::types::Value::Null,
            (_, Value::String(s)) if s.is_empty() && !matches!(k.as_str(), "title" | "body" | "status") => {
                rusqlite::types::Value::Null
            }
            (_, Value::String(s)) => s.clone().into(),
            _ => bail!("{k} must be a string"),
        };
        if k == "status" {
            check_status(v.as_str().unwrap_or_default())?;
        }
        if k == "title" && v.as_str().is_none_or(|s| s.trim().is_empty()) {
            bail!("title required");
        }
        if k == "project" {
            if let Value::String(p) = v {
                if !p.is_empty() {
                    ensure_project(conn, p)?;
                }
            }
        }
        conn.execute(&format!("UPDATE tickets SET {k}=?, updated=? WHERE key=?"), params![sql_val, now_iso(), key])?;
        if k == "status" && before.status != v.as_str().unwrap_or_default() {
            event(conn, key, session, "status", &format!("{} → {}", before.status, v.as_str().unwrap_or_default()))?;
        }
    }
    get(conn, key)
}

pub fn claim(conn: &Connection, key: &str, session: &str) -> Result<()> {
    get(conn, key)?;
    conn.execute(
        "UPDATE tickets SET status='doing', session=?, updated=? WHERE key=?",
        params![session, now_iso(), key],
    )?;
    event(conn, key, Some(session), "claimed", "")
}

/// Closing records the summary, the branch and the commits since the session's start HEAD.
pub fn close(conn: &Connection, key: &str, session: Option<&str>, summary: &str) -> Result<()> {
    get(conn, key)?;
    let sess: Option<(Option<String>, Option<String>)> = match session {
        Some(s) => conn
            .query_row("SELECT head_start, cwd FROM sessions WHERE id=?", [s], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?,
        None => None,
    };
    let (mut branch, mut commits) = (String::new(), String::new());
    if let Some((head, Some(cwd))) = sess {
        branch = git(&cwd, &["branch", "--show-current"]);
        if let Some(h) = head.filter(|h| !h.is_empty()) {
            commits = git(&cwd, &["log", "--oneline", &format!("{h}..HEAD")]);
        }
    }
    conn.execute("UPDATE tickets SET status='done', updated=? WHERE key=?", params![now_iso(), key])?;
    let mut parts = vec![];
    if !summary.is_empty() {
        parts.push(summary.to_string());
    }
    if !branch.is_empty() {
        parts.push(format!("branch: {branch}"));
    }
    if !commits.is_empty() {
        parts.push(format!("commits:\n{commits}"));
    }
    event(conn, key, session, "closed", &parts.join("\n\n"))
}

/// `todo` with no blocker or a done blocker, by rank.
pub fn ready(conn: &Connection, repo: Option<&str>) -> Result<Vec<Ticket>> {
    let cols = COLS.split(',').map(|c| format!("i.{c}")).collect::<Vec<_>>().join(",");
    let mut st = conn.prepare(&format!(
        "SELECT {cols} FROM tickets i LEFT JOIN tickets b ON b.key=i.blocked_by
         WHERE i.status='todo' AND (b.key IS NULL OR b.status='done') AND (?1 IS NULL OR i.repo=?1) ORDER BY i.rank"
    ))?;
    let v = st.query_map([repo], Ticket::from_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(v)
}

pub fn links(conn: &Connection, key: &str) -> Result<Vec<Value>> {
    crate::db::rows(conn, "SELECT kind, ref, title, path, why, at FROM links WHERE ticket=? ORDER BY at", [key])
}

pub fn add_link(conn: &Connection, key: &str, kind: &str, r: &str, title: &str, path: &str, why: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO links VALUES(?,?,?,?,?,?,?)",
        params![key, kind, r, title, path, why, now_iso()],
    )?;
    Ok(())
}

pub fn link_case(conn: &Connection, key: &str, case_id: &str, title: &str, url: Option<&str>) -> Result<()> {
    get(conn, key)?;
    add_link(conn, key, "waypoint", case_id, title, url.unwrap_or(""), "")?;
    event(conn, key, None, "context", &format!("Waypoint case: {title}"))
}

/// Tickets a session worked on, as ticket keys, doing first.
pub fn ticket_of_session(conn: &Connection, session: &str) -> Result<Option<Ticket>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM tickets WHERE session=? ORDER BY status='doing' DESC"),
            [session],
            Ticket::from_row,
        )
        .optional()?)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::db::temp_db;
    use serde_json::json;

    pub fn new(repo: &str, title: &str) -> NewTicket {
        NewTicket { repo: repo.into(), title: title.into(), ..Default::default() }
    }

    #[test]
    fn keys_use_three_letters_and_count_per_prefix() {
        let (_d, c) = temp_db();
        assert_eq!(create(&c, new("demo-repo", "a")).unwrap(), "DEM-1");
        assert_eq!(create(&c, new("demo-repo", "b")).unwrap(), "DEM-2");
        assert_eq!(create(&c, new("wishlang", "c")).unwrap(), "WIS-1");
        // another repo with the same prefix continues the sequence so keys stay unique
        assert_eq!(create(&c, new("demolition", "d")).unwrap(), "DEM-3");
        assert_eq!(create(&c, new("42", "e")).unwrap(), "TKT-1");
    }

    #[test]
    fn ready_respects_blockers_and_rank() {
        let (_d, c) = temp_db();
        let a = create(&c, new("r", "first")).unwrap();
        let b = create(&c, NewTicket { blocked_by: Some(a.clone()), ..new("r", "second") }).unwrap();
        let d = create(&c, new("r", "third")).unwrap();
        create(&c, NewTicket { status: Some("backlog".into()), ..new("r", "not ready") }).unwrap();
        let keys = |c: &Connection| ready(c, None).unwrap().into_iter().map(|t| t.key).collect::<Vec<_>>();
        assert_eq!(keys(&c), vec![a.clone(), d.clone()]);
        update(&c, &a, json!({"status":"done"}).as_object().unwrap(), None).unwrap();
        assert_eq!(keys(&c), vec![b.clone(), d.clone()]);
        update(&c, &d, json!({"rank": 0.5}).as_object().unwrap(), None).unwrap();
        assert_eq!(keys(&c), vec![d, b]);
        assert!(ready(&c, Some("other")).unwrap().is_empty());
    }

    #[test]
    fn status_changes_are_logged_and_validated() {
        let (_d, c) = temp_db();
        let k = create(&c, new("r", "t")).unwrap();
        update(&c, &k, json!({"status":"doing"}).as_object().unwrap(), Some("s1")).unwrap();
        let ev = events(&c, &k).unwrap();
        assert_eq!(ev.last().unwrap()["text"], "todo → doing");
        assert_eq!(ev.last().unwrap()["session"], "s1");
        assert!(update(&c, &k, json!({"status":"nope"}).as_object().unwrap(), None).is_err());
        assert!(update(&c, &k, json!({"priority":9}).as_object().unwrap(), None).is_err());
        // assigning a project creates it; empty string clears the field
        update(&c, &k, json!({"project":"P","cluster":"x"}).as_object().unwrap(), None).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM projects WHERE name='P'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let t = update(&c, &k, json!({"cluster":""}).as_object().unwrap(), None).unwrap();
        assert_eq!(t.cluster, None);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn claim_and_close_record_branch_and_commits() {
        let (d, c) = temp_db();
        let repo = d.path().join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let cwd = repo.to_string_lossy().to_string();
        let head = git(&cwd, &["rev-parse", "HEAD"]);
        git_in(&repo, &["checkout", "-q", "-b", "feat/x"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "did the thing"]);
        c.execute("INSERT INTO sessions(id,repo,cwd,head_start) VALUES('s1','proj',?,?)", params![cwd, head]).unwrap();
        let k = create(&c, new("proj", "t")).unwrap();
        claim(&c, &k, "s1").unwrap();
        let t = get(&c, &k).unwrap();
        assert_eq!((t.status.as_str(), t.session.as_deref()), ("doing", Some("s1")));
        close(&c, &k, Some("s1"), "all done").unwrap();
        assert_eq!(get(&c, &k).unwrap().status, "done");
        let text = events(&c, &k).unwrap().last().unwrap()["text"].as_str().unwrap().to_string();
        assert!(
            text.contains("all done") && text.contains("branch: feat/x") && text.contains("did the thing"),
            "{text}"
        );
        assert!(!text.contains("base"));
    }

    #[test]
    fn worktrees_resolve_to_main_repo() {
        let (d, c) = temp_db();
        let repo = d.path().join("mainrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(&repo, &["init", "-q", "-b", "main"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let wt = d.path().join("wt");
        git_in(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wtb"]);
        assert_eq!(repo_at(&c, wt.to_str().unwrap()).unwrap(), "mainrepo");
        let p = repo_path(&c, "mainrepo").unwrap().unwrap();
        assert!(p.ends_with("mainrepo"));
    }
}
