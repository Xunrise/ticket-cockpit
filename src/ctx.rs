//! Everything a tool needs from the outside world, so tests can point it at temp files and private tmux sockets.
use crate::tmux::Tmux;
use crate::{paths, tickets};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Ctx {
    pub db_path: PathBuf,
    pub tmux: Tmux,
    pub palaver_bin: PathBuf,
    pub context_dir: PathBuf,
    pub claude_settings: PathBuf,
    pub coach_file: PathBuf,
    /// How Claude sessions should invoke the CLI (an absolute path).
    pub exe: String,
    /// Latest call and coach baseline; the daemon refreshes it, other processes see the baseline only.
    pub calls: Arc<Mutex<Value>>,
    /// Latest update check (the daemon refreshes it every 10 minutes).
    pub update: Arc<Mutex<Value>>,
}

impl Ctx {
    pub fn from_env() -> Self {
        let coach_file = paths::coach_baseline();
        Ctx {
            db_path: paths::db_path(),
            tmux: Tmux::system(),
            palaver_bin: paths::palaver_bin(),
            context_dir: paths::context_dir(),
            claude_settings: paths::claude_settings(),
            calls: Arc::new(Mutex::new(json!({"last": null, "baseline": crate::palaver::coach_baseline(&coach_file)}))),
            coach_file,
            update: Arc::new(Mutex::new(Value::Null)),
            exe: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "cockpit".into()),
        }
    }

    /// A context rooted in `dir` with a private tmux pair; the Palaver binary defaults to a missing path.
    pub fn for_test(dir: &std::path::Path, tmux: Tmux) -> Self {
        Ctx {
            db_path: dir.join("cockpit.db"),
            tmux,
            palaver_bin: dir.join("no-palaver"),
            context_dir: dir.join("context"),
            claude_settings: dir.join("claude-settings.json"),
            coach_file: dir.join("coach.json"),
            exe: "cockpit".into(),
            calls: Arc::new(Mutex::new(json!({"last": null, "baseline": null}))),
            update: Arc::new(Mutex::new(Value::Null)),
        }
    }

    pub fn db(&self) -> Result<Connection> {
        crate::db::open(&self.db_path)
    }

    pub fn repo_path(&self, conn: &Connection, repo: &str) -> Result<Option<String>> {
        tickets::repo_path(conn, repo)
    }
}
