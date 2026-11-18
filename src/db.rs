//! SQLite access: WAL so the hook (a separate process) and the daemon write concurrently,
//! plus versioned migrations through `PRAGMA user_version`.
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE tickets(key TEXT PRIMARY KEY, repo TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
       status TEXT NOT NULL DEFAULT 'todo', priority INTEGER NOT NULL DEFAULT 3, project TEXT, cluster TEXT, blocked_by TEXT,
       session TEXT, rank REAL NOT NULL DEFAULT 0, created TEXT NOT NULL, updated TEXT NOT NULL);
     CREATE INDEX tickets_project ON tickets(project);
     CREATE TABLE sessions(id TEXT PRIMARY KEY, repo TEXT, cwd TEXT, started TEXT, last_seen TEXT, ended TEXT,
       head_start TEXT, head_end TEXT, tmux_pane TEXT, state TEXT, state_detail TEXT, state_at TEXT,
       transcript TEXT, last_message TEXT, last_prompt TEXT, trail TEXT);
     CREATE TABLE events(id INTEGER PRIMARY KEY, ticket TEXT, session TEXT, kind TEXT NOT NULL, text TEXT NOT NULL DEFAULT '', at TEXT NOT NULL);
     CREATE INDEX events_ticket ON events(ticket);
     CREATE TABLE repos(name TEXT PRIMARY KEY, path TEXT NOT NULL);
     CREATE TABLE projects(name TEXT PRIMARY KEY, palaver TEXT, services TEXT);
     CREATE TABLE prompts(session TEXT NOT NULL, at TEXT NOT NULL);
     CREATE INDEX prompts_at ON prompts(at);
     CREATE TABLE links(ticket TEXT, kind TEXT, ref TEXT, title TEXT, path TEXT, why TEXT, at TEXT, PRIMARY KEY(ticket, kind, ref));
     CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    // v2: a project's home repo, so planned tickets can be added before the project has any
    "ALTER TABLE projects ADD COLUMN repo TEXT;
     UPDATE projects SET repo = name WHERE repo IS NULL AND name IN (SELECT name FROM repos);",
    // v3: the error log, and close-out requests (wrap up, then close the window once the ticket is done)
    "CREATE TABLE errors(id INTEGER PRIMARY KEY, at TEXT NOT NULL, source TEXT NOT NULL, text TEXT NOT NULL, context TEXT NOT NULL DEFAULT '');
     CREATE TABLE close_requests(ticket TEXT PRIMARY KEY, session TEXT, at TEXT NOT NULL);",
];

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_millis(2000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn migrate(conn: &Connection) -> Result<()> {
    let have: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(have) {
        conn.execute_batch(&format!("BEGIN;{sql};PRAGMA user_version={};COMMIT;", i + 1))?;
    }
    Ok(())
}

#[cfg(test)]
pub fn temp_db() -> (tempdir::Dir, Connection) {
    let d = tempdir::Dir::new();
    let c = open(&d.path().join("t.db")).unwrap();
    (d, c)
}

#[cfg(test)]
pub mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    pub struct Dir(PathBuf);

    #[allow(clippy::new_without_default)]
    impl Dir {
        pub fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "cockpit-t-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_once_and_uses_wal() {
        let (d, c) = temp_db();
        let v: usize = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len());
        let mode: String = c.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        drop(c);
        open(&d.path().join("t.db")).unwrap(); // reopening is a no-op
    }
}

/// Runs a query and returns every row as a JSON object keyed by column name.
pub fn rows(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<serde_json::Value>> {
    use rusqlite::types::ValueRef;
    let mut st = conn.prepare(sql)?;
    let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let out = st
        .query_map(params, |r| {
            let mut o = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                o.insert(
                    n.clone(),
                    match r.get_ref(i)? {
                        ValueRef::Null => serde_json::Value::Null,
                        ValueRef::Integer(v) => v.into(),
                        ValueRef::Real(v) => v.into(),
                        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
                        ValueRef::Blob(_) => serde_json::Value::Null,
                    },
                );
            }
            Ok(serde_json::Value::Object(o))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}
