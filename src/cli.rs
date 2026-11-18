//! Command line: ticket shortcuts for Claude sessions (direct SQLite) and helpers shared by other subcommands.
use crate::tickets::{self, NewTicket};
use crate::{db, paths};
use anyhow::{anyhow, bail, Result};

/// Removes `--name value` from args and returns the value.
pub fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == &format!("--{name}"))?;
    if i + 1 >= args.len() {
        args.remove(i);
        return None;
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}

pub fn switch(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == &format!("--{name}")) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

fn cwd() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into())
}

/// `add`, `ready`, `show`, `claim`, `comment`, `close`. `args[0]` is the subcommand.
pub fn ticket_command(mut args: Vec<String>) -> Result<()> {
    let conn = db::open(&paths::db_path())?;
    let cmd = args.remove(0);
    match cmd.as_str() {
        "add" => {
            let repo = match flag(&mut args, "repo") {
                Some(r) => r,
                None => tickets::repo_at(&conn, &cwd())?,
            };
            let project = flag(&mut args, "project");
            let blocked_by = flag(&mut args, "blocked-by");
            let cluster = flag(&mut args, "cluster");
            let priority = flag(&mut args, "priority").map(|p| p.parse()).transpose()?;
            let title = args.first().cloned().ok_or_else(|| anyhow!("usage: cockpit add \"title\" [\"body\"]"))?;
            let body = args.get(1).cloned().unwrap_or_default();
            let key = tickets::create(
                &conn,
                NewTicket { repo, title, body, project, cluster, blocked_by, priority, status: None },
            )?;
            println!("{key}");
        }
        "ready" => {
            let repo = if switch(&mut args, "all") { None } else { Some(tickets::repo_at(&conn, &cwd())?) };
            for t in tickets::ready(&conn, repo.as_deref())? {
                println!("{}\tP{}\t{}", t.key, t.priority, t.title);
            }
        }
        "show" => {
            let key = args.first().ok_or_else(|| anyhow!("usage: cockpit show KEY"))?;
            let t = tickets::get(&conn, key)?;
            println!("{} [{}] {}\n\n{}", t.key, t.status, t.title, t.body);
        }
        "claim" => {
            let session = flag(&mut args, "session").ok_or_else(|| anyhow!("--session required"))?;
            let key = args.first().ok_or_else(|| anyhow!("usage: cockpit claim KEY --session ID"))?;
            tickets::claim(&conn, key, &session)?;
            println!("claimed {key}");
        }
        "comment" => {
            let session = flag(&mut args, "session");
            let (key, text) = (args.first(), args.get(1));
            let (Some(key), Some(text)) = (key, text) else { bail!("usage: cockpit comment KEY \"text\"") };
            tickets::get(&conn, key)?;
            tickets::event(&conn, key, session.as_deref(), "comment", text)?;
            println!("ok");
        }
        "close" => {
            let session = flag(&mut args, "session");
            let summary = flag(&mut args, "summary").unwrap_or_default();
            let key = args.first().ok_or_else(|| anyhow!("usage: cockpit close KEY --summary \"…\""))?;
            tickets::close(&conn, key, session.as_deref(), &summary)?;
            println!("closed {key}");
        }
        other => bail!("unknown command {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_removed_from_args() {
        let mut a: Vec<String> = ["add", "--repo", "x", "title", "--all"].iter().map(|s| s.to_string()).collect();
        assert_eq!(flag(&mut a, "repo").as_deref(), Some("x"));
        assert!(switch(&mut a, "all"));
        assert_eq!(a, ["add", "title"]);
        assert_eq!(flag(&mut a, "nope"), None);
    }
}
