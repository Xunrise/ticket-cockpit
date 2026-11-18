//! Updates: what is installed, whether something newer exists (local checkout or GitHub release),
//! and the flows that apply it (`cockpit update | reload | rollback`).
pub mod local;

use crate::ctx::Ctx;
use crate::settings;
use anyhow::Result;
use serde_json::{json, Value};

/// The commit this binary was built from; `COCKPIT_GIT_SHA` overrides it (demos, tests).
pub fn installed_commit() -> String {
    std::env::var("COCKPIT_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("COCKPIT_GIT_SHA").to_string())
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `0.2.0 (abc123def456)`.
pub fn installed() -> String {
    format!("{} ({})", version(), installed_commit())
}

pub(crate) fn setting(ctx: &Ctx, key: &str) -> String {
    ctx.db()
        .ok()
        .and_then(|c| settings::get(&c, key).ok())
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// What an update would bring, per channel. Disabled by `check_updates`.
pub fn check(ctx: &Ctx) -> Result<Value> {
    let conn = ctx.db()?;
    let channel = setting(ctx, "update_channel");
    let installed_commit = installed_commit();
    let mut st = if !settings::get_bool(&conn, "check_updates") {
        json!({"available": null, "commits_ahead": 0, "behind_remote": 0, "summary": "update checks are off", "notes": ""})
    } else {
        local::status(&setting(ctx, "cockpit_repo"), &installed_commit)
    };
    st["channel"] = channel.into();
    st["installed"] = installed().into();
    st["installed_commit"] = installed_commit.into();
    st["installed_version"] = version().into();
    st["checked_at"] = crate::util::now_iso().into();
    Ok(st)
}

/// Stores a fresh check for `cockpit.update_status` and the summary. Never fails: a failed check keeps the old one.
pub fn refresh(ctx: &Ctx) {
    match check(ctx) {
        Ok(st) => {
            if let Ok(mut g) = ctx.update.lock() {
                *g = st;
            }
        }
        Err(e) => crate::errors::log_quiet(ctx, "daemon", &format!("update check: {e:#}"), ""),
    }
}

/// The cached check (computed on first use, or when `force`).
pub fn status(ctx: &Ctx, force: bool) -> Result<Value> {
    let cached = ctx.update.lock().map(|g| g.clone()).unwrap_or(Value::Null);
    if force || cached.is_null() {
        refresh(ctx);
    }
    Ok(ctx.update.lock().map(|g| g.clone()).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use crate::tmux::Tmux;

    #[test]
    fn update_status_is_checked_once_then_cached_and_summarised() {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("update-unit"));
        let conn = c.db().unwrap();
        settings::set(&conn, "cockpit_repo", &json!(d.path().join("none").to_string_lossy())).unwrap();
        let s = crate::tools::call(&c, "cockpit.update_status", json!({})).unwrap();
        assert_eq!(s["channel"], "local");
        assert!(s["summary"].as_str().unwrap().contains("not a git checkout"));
        assert_eq!(s["installed"], installed().as_str());
        // cached: a changed setting shows only on refresh
        settings::set(&conn, "check_updates", &json!(false)).unwrap();
        assert!(status(&c, false).unwrap()["summary"].as_str().unwrap().contains("not a git checkout"));
        assert_eq!(status(&c, true).unwrap()["summary"], "update checks are off");
        let sum = crate::summary::summary(&c).unwrap();
        assert_eq!(sum["update"], json!({"available": false, "commits": 0}));
    }

    #[test]
    fn the_summary_reports_waiting_commits() {
        let d = Dir::new();
        let c = Ctx::for_test(d.path(), Tmux::private("update-unit2"));
        *c.update.lock().unwrap() = json!({"available": "abc", "commits_ahead": 3});
        assert_eq!(crate::summary::summary(&c).unwrap()["update"], json!({"available": true, "commits": 3}));
    }
}
