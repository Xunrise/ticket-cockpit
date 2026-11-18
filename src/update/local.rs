//! The local channel: the cockpit is built from `main` of a checkout (`cockpit_repo`).
use crate::util::{git, now_ms};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// `git fetch` runs at most once an hour per checkout.
static FETCHED: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);
const FETCH_EVERY_MS: i64 = 3_600_000;

fn count(repo: &str, range: &str) -> i64 {
    git(repo, &["rev-list", "--count", range]).parse().unwrap_or(0)
}

fn fetch_due(repo: &str) -> bool {
    let now = now_ms();
    let mut g = FETCHED.lock().unwrap_or_else(|e| e.into_inner());
    let last = g.get_or_insert_with(HashMap::new).entry(repo.to_string()).or_insert(0);
    if now - *last > FETCH_EVERY_MS {
        *last = now;
        return true;
    }
    false
}

/// Commits on `main` since the installed commit, and whether `main` is behind `origin/main` (pull first).
pub fn status(repo: &str, installed: &str) -> Value {
    if !Path::new(repo).join(".git").exists() {
        return json!({"available": null, "commits_ahead": 0, "behind_remote": 0, "summary": format!("cockpit_repo {repo} is not a git checkout"), "notes": ""});
    }
    let has_remote = !git(repo, &["remote"]).is_empty();
    if has_remote && fetch_due(repo) {
        let _ = git(repo, &["fetch", "--quiet"]);
    }
    let ahead = if installed == "unknown" { 0 } else { count(repo, &format!("{installed}..main")) };
    let behind = if has_remote { count(repo, "main..origin/main") } else { 0 };
    let head = git(repo, &["rev-parse", "--short=12", "main"]);
    let notes =
        if ahead > 0 { git(repo, &["log", "--oneline", "-30", &format!("{installed}..main")]) } else { String::new() };
    let mut summary = match ahead {
        0 => "up to date".to_string(),
        1 => "update available: 1 commit".to_string(),
        n => format!("update available: {n} commits"),
    };
    if behind > 0 {
        summary += &format!("; origin/main has {behind} more: pull first");
    }
    json!({"head": head, "available": (ahead > 0).then_some(head.clone()), "commits_ahead": ahead, "behind_remote": behind, "summary": summary, "notes": notes})
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What the local channel runs: build and install from the checkout, then re-register with `cockpit install`.
pub fn install_script(repo: &str, exe: &str) -> String {
    format!("cd {} && git log --oneline -1 && cargo install --path . && {} install", quote(repo), quote(exe))
}

pub fn steps(repo: &str) -> Vec<String> {
    vec![format!("cd {repo}"), "git log --oneline -1".into(), "cargo install --path .".into(), "cockpit install".into()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::util::sh;

    pub fn g(dir: &Path, args: &[&str]) -> String {
        let mut a = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
        a.extend(args);
        sh("git", &a, Some(dir))
    }

    pub fn commit(dir: &Path, msg: &str) -> String {
        g(dir, &["commit", "-q", "--allow-empty", "-m", msg]);
        g(dir, &["rev-parse", "--short=12", "HEAD"])
    }

    #[test]
    fn counts_commits_since_the_installed_one() {
        let d = Dir::new();
        let repo = d.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        let installed = commit(&repo, "first");
        let up = status(repo.to_str().unwrap(), &installed);
        assert_eq!(
            (up["available"].is_null(), up["commits_ahead"].as_i64(), up["summary"].as_str()),
            (true, Some(0), Some("up to date"))
        );
        commit(&repo, "second");
        let head = commit(&repo, "third");
        let s = status(repo.to_str().unwrap(), &installed);
        assert_eq!(s["commits_ahead"], 2);
        assert_eq!(s["available"], head.as_str());
        assert_eq!(s["summary"], "update available: 2 commits");
        assert!(s["notes"].as_str().unwrap().contains("third") && s["notes"].as_str().unwrap().contains("second"));
        assert_eq!(status(repo.to_str().unwrap(), "unknown")["commits_ahead"], 0, "unknown build: no claim");
    }

    #[test]
    fn a_checkout_behind_origin_says_pull_first() {
        let d = Dir::new();
        let (origin, a, b) = (d.path().join("origin.git"), d.path().join("a"), d.path().join("b"));
        sh("git", &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()], None);
        sh("git", &["clone", "-q", origin.to_str().unwrap(), a.to_str().unwrap()], None);
        g(&a, &["checkout", "-q", "-b", "main"]);
        let installed = commit(&a, "first");
        g(&a, &["push", "-q", "origin", "main"]);
        sh("git", &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()], None);
        // someone else pushes; b has not pulled
        commit(&a, "elsewhere");
        g(&a, &["push", "-q", "origin", "main"]);
        let s = status(b.to_str().unwrap(), &installed);
        assert_eq!(s["behind_remote"], 1, "{s}");
        assert_eq!(s["commits_ahead"], 0);
        assert!(s["summary"].as_str().unwrap().contains("pull first"), "{s}");
    }

    #[test]
    fn a_folder_that_is_not_a_checkout_is_reported() {
        let d = Dir::new();
        let s = status(d.path().to_str().unwrap(), "abc");
        assert!(s["summary"].as_str().unwrap().contains("not a git checkout"));
    }
}
