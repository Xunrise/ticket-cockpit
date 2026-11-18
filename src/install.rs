//! `cockpit install` / `cockpit uninstall`: tmux config, launchd agent and the global Claude hooks.
//! Idempotent; every step says what it changed. Everything derives from `HOME`, so tests run on a temp one.
use crate::cli::switch;
use crate::daemon::{self, LABEL};
use crate::{paths, tui, util};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

pub const HOOK_EVENTS: [&str; 7] =
    ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop", "SessionEnd"];
const PLIST: &str = include_str!("../assets/io.cockpit.daemon.plist");
const PROTO_HOOK: &str = "ticket-cockpit-proto/tk.mjs hook";
const HOOK_TIMEOUT: i64 = 5;

fn is_proto(cmd: &str) -> bool {
    cmd.contains(PROTO_HOOK)
}

/// `<anything>/cockpit hook`, the same test the connections tab uses.
fn is_cockpit(cmd: &str) -> bool {
    let mut words = cmd.split_whitespace().rev();
    words.next() == Some("hook")
        && words.next().is_some_and(|p| p.trim_matches('\'').rsplit('/').next() == Some("cockpit"))
}

fn quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+@:~".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn indent_of(text: &str) -> String {
    text.lines()
        .skip(1)
        .find(|l| l.starts_with([' ', '\t']))
        .map_or("  ".into(), |l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect())
}

fn render(v: &Value, original: &str) -> Result<String> {
    let mut buf = Vec::new();
    let indent = indent_of(original);
    let fmt = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    serde::Serialize::serialize(v, &mut serde_json::Serializer::with_formatter(&mut buf, fmt))?;
    let mut s = String::from_utf8(buf)?;
    if original.ends_with('\n') || original.is_empty() {
        s.push('\n');
    }
    Ok(s)
}

fn hook_entry(cmd: &str) -> Value {
    json!({"hooks": [{"type": "command", "command": cmd, "timeout": HOOK_TIMEOUT}]})
}

/// Visits every hook object of an event's groups; `f` returns false to remove it. Empty groups go with it.
fn retain_hooks(groups: &mut Vec<Value>, mut f: impl FnMut(&mut Map<String, Value>) -> bool) -> bool {
    let mut touched = false;
    for g in groups.iter_mut() {
        if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
            let before = hs.len();
            hs.retain_mut(|h| h.as_object_mut().is_none_or(&mut f));
            touched |= hs.len() != before;
        }
    }
    let before = groups.len();
    groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|h| !h.is_empty()));
    touched || groups.len() != before
}

/// The settings text after installing the cockpit hook on every event, plus a line per event.
pub fn install_hooks(text: &str, exe: &str) -> Result<(String, Vec<String>)> {
    let cmd = format!("{} hook", quote(exe));
    let mut root: Value = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(text)? };
    let obj = root.as_object_mut().ok_or_else(|| anyhow!("settings.json is not a JSON object"))?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| anyhow!("\"hooks\" in settings.json is not an object"))?;
    let mut lines = vec![];
    for ev in HOOK_EVENTS {
        let groups = hooks.entry(ev).or_insert_with(|| json!([]));
        let groups = groups.as_array_mut().ok_or_else(|| anyhow!("hooks.{ev} is not an array"))?;
        let mut have = false; // a cockpit hook is already kept for this event
        let (mut replaced, mut updated, mut dropped) = (0, false, 0);
        // pass 1: existing cockpit entries (fix their command, drop duplicates)
        retain_hooks(groups, |h| {
            if !h.get("command").and_then(Value::as_str).is_some_and(is_cockpit) {
                return true;
            }
            if have {
                dropped += 1;
                return false;
            }
            have = true;
            if h.get("command").and_then(Value::as_str) != Some(&cmd) || h.get("timeout") != Some(&json!(HOOK_TIMEOUT))
            {
                h.insert("command".into(), cmd.clone().into());
                h.insert("timeout".into(), HOOK_TIMEOUT.into());
                updated = true;
            }
            true
        });
        // pass 2: prototype entries become the cockpit entry (first one, in place) or vanish
        retain_hooks(groups, |h| {
            if !h.get("command").and_then(Value::as_str).is_some_and(is_proto) {
                return true;
            }
            replaced += 1;
            if have {
                return false;
            }
            have = true;
            h.insert("command".into(), cmd.clone().into());
            h.insert("timeout".into(), HOOK_TIMEOUT.into());
            true
        });
        let mut notes = vec![];
        if replaced > 0 {
            notes.push(format!("replaced {replaced} prototype tk.mjs hook(s)"));
        }
        if updated {
            notes.push("updated the command".into());
        }
        if dropped > 0 {
            notes.push(format!("removed {dropped} duplicate(s)"));
        }
        if !have {
            groups.push(hook_entry(&cmd));
            notes.push("added".into());
        }
        lines.push(if notes.is_empty() {
            format!("hooks.{ev}: unchanged")
        } else {
            format!("hooks.{ev}: {}", notes.join(", "))
        });
    }
    Ok((render(&root, text)?, lines))
}

/// The settings text without any cockpit hook, plus a line per touched event.
pub fn remove_hooks(text: &str) -> Result<(String, Vec<String>)> {
    let mut root: Value = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(text)? };
    let mut lines = vec![];
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        let mut emptied = vec![];
        for (ev, groups) in hooks.iter_mut() {
            let Some(groups) = groups.as_array_mut() else { continue };
            let mut n = 0;
            if retain_hooks(groups, |h| {
                let hit = h.get("command").and_then(Value::as_str).is_some_and(is_cockpit);
                n += hit as usize;
                !hit
            }) {
                lines.push(format!("hooks.{ev}: removed {n} cockpit hook(s)"));
                if groups.is_empty() {
                    emptied.push(ev.clone());
                }
            }
        }
        for ev in emptied {
            hooks.shift_remove(&ev);
        }
    }
    Ok((render(&root, text)?, lines))
}

fn plist(exe: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let home = paths::home();
    let path = format!("{0}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", home.display());
    PLIST
        .replace("@LABEL@", LABEL)
        .replace("@EXE@", &esc(exe))
        .replace("@HOME@", &esc(&home.to_string_lossy()))
        .replace("@PATH@", &esc(&path))
        .replace("@LOG@", &esc(&paths::log_dir().join("daemon.log").to_string_lossy()))
}

fn write_if_changed(path: &Path, content: &str, dry: bool) -> Result<&'static str> {
    let state = match std::fs::read_to_string(path) {
        Ok(old) if old == content => return Ok("unchanged"),
        Ok(_) => "updated",
        Err(_) => "created",
    };
    if !dry {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(state)
}

fn say(dry: bool, line: String) {
    println!("{}{line}", if dry { "(dry-run) " } else { "" });
}

/// Backs the file up next to itself, then replaces it atomically.
fn write_settings(path: &Path, new: &str) -> Result<String> {
    let backup = path.with_file_name(format!(
        "{}.cockpit-backup-{}",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        util::now_iso().replace([':', '.'], "-")
    ));
    if path.exists() {
        std::fs::copy(path, &backup)?;
    } else if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.cockpit-tmp");
    std::fs::write(&tmp, new)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path)?;
    Ok(backup.display().to_string())
}

fn settings_step(dry: bool, edit: impl Fn(&str) -> Result<(String, Vec<String>)>) -> Result<()> {
    let path = paths::claude_settings();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let (new, lines) = edit(&old).with_context(|| format!("{} is not usable, left untouched", path.display()))?;
    for l in &lines {
        say(dry, format!("  {l}"));
    }
    if new == old {
        say(dry, format!("Claude settings {}: unchanged", path.display()));
    } else if dry {
        say(dry, format!("Claude settings {}: would be rewritten (backup alongside)", path.display()));
    } else {
        let backup = write_settings(&path, &new)?;
        println!("Claude settings {}: rewritten, backup {backup}", path.display());
    }
    Ok(())
}

pub fn install(dry: bool) -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut failed = None;

    let conf = paths::tmux_conf();
    let st = write_if_changed(&conf, tui::TMUX_CONF, dry)?;
    say(
        dry,
        format!(
            "tmux config {}: {}",
            conf.display(),
            if dry && st != "unchanged" { format!("would be {st}") } else { st.into() }
        ),
    );

    let plist_path = paths::launch_agent_plist();
    let st = write_if_changed(&plist_path, &plist(&exe), dry)?;
    if !dry {
        std::fs::create_dir_all(paths::log_dir())?;
    }
    say(
        dry,
        format!(
            "launchd agent {}: {}",
            plist_path.display(),
            if dry && st != "unchanged" { format!("would be {st}") } else { st.into() }
        ),
    );
    let loaded = daemon::loaded();
    let target = daemon::target();
    let plist_arg = plist_path.to_string_lossy().into_owned();
    let mut agent = |steps: &[&[&str]]| {
        for s in steps {
            if dry {
                say(dry, format!("  launchctl {}", s.join(" ")));
            } else if let Err(e) = daemon::launchctl(s) {
                failed = Some(e);
                return;
            } else {
                println!("  launchctl {}", s.join(" "));
            }
        }
    };
    match (loaded, st) {
        (true, "unchanged") => {
            say(dry, format!("launchd agent {LABEL}: already loaded (`cockpit daemon restart` picks up a new binary)"))
        }
        (true, _) => {
            agent(&[&["bootout", &target], &["bootstrap", &daemon::domain(), &plist_arg], &["kickstart", &target]])
        }
        (false, _) => agent(&[&["bootstrap", &daemon::domain(), &plist_arg], &["kickstart", &target]]),
    }

    settings_step(dry, |t| install_hooks(t, &exe))?;
    match failed {
        Some(e) => Err(e.context("the launchd agent could not be loaded (everything else is done)")),
        None => Ok(()),
    }
}

pub fn uninstall(dry: bool) -> Result<()> {
    let target = daemon::target();
    if daemon::loaded() {
        if !dry {
            daemon::launchctl(&["bootout", &target])?;
        }
        say(dry, format!("launchd agent {LABEL}: booted out (launchctl bootout {target})"));
    } else {
        say(dry, format!("launchd agent {LABEL}: not loaded"));
    }
    let plist_path = paths::launch_agent_plist();
    if plist_path.exists() {
        if !dry {
            std::fs::remove_file(&plist_path)?;
        }
        say(dry, format!("launchd agent {}: deleted", plist_path.display()));
    } else {
        say(dry, format!("launchd agent {}: not present", plist_path.display()));
    }
    if paths::claude_settings().exists() {
        settings_step(dry, remove_hooks)?;
    } else {
        say(dry, format!("Claude settings {}: not present", paths::claude_settings().display()));
    }
    println!("kept: {} and {}", paths::config_dir().display(), paths::data_dir().display());
    Ok(())
}

/// `install` / `uninstall` with `--dry-run`.
pub fn command(mut args: Vec<String>) -> Result<()> {
    let sub = args.remove(0);
    let dry = switch(&mut args, "dry-run");
    if let Some(a) = args.first() {
        bail!("unexpected argument {a}");
    }
    if sub == "install" {
        install(dry)
    } else {
        uninstall(dry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/bin/cockpit";

    fn events(v: &str, ev: &str) -> Vec<String> {
        let v: Value = serde_json::from_str(v).unwrap();
        v["hooks"][ev]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter().map(|h| h["command"].as_str().unwrap().to_string()))
            .collect()
    }

    const SETTINGS: &str = r#"{
    "model": "opus",
    "hooks": {
        "Stop": [
            {"hooks": [{"type": "command", "command": "other.py Stop"}]},
            {"hooks": [{"type": "command", "command": "node /x/ticket-cockpit-proto/tk.mjs hook", "timeout": 5}]}
        ],
        "PostToolUseFailure": [{"hooks": [{"type": "command", "command": "f.py"}]}]
    },
    "theme": "dark"
}
"#;

    #[test]
    fn proto_hook_is_replaced_in_place_and_the_rest_is_kept() {
        let (out, lines) = install_hooks(SETTINGS, EXE).unwrap();
        assert_eq!(events(&out, "Stop"), ["other.py Stop", "/bin/cockpit hook"]);
        assert!(lines.contains(&"hooks.Stop: replaced 1 prototype tk.mjs hook(s)".to_string()));
        assert!(lines.contains(&"hooks.PostToolUse: added".to_string()));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), ["model", "hooks", "theme"]);
        assert_eq!(v["hooks"].as_object().unwrap().keys().collect::<Vec<_>>()[..2], ["Stop", "PostToolUseFailure"]);
        assert!(out.starts_with("{\n    \"model\""), "indent of the original is kept");
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn install_twice_changes_nothing() {
        let (once, _) = install_hooks(SETTINGS, EXE).unwrap();
        let (twice, lines) = install_hooks(&once, EXE).unwrap();
        assert_eq!(once, twice);
        assert!(lines.iter().all(|l| l.ends_with("unchanged")), "{lines:?}");
    }

    #[test]
    fn a_moved_binary_updates_the_command_without_duplicates() {
        let (once, _) = install_hooks(SETTINGS, EXE).unwrap();
        let (moved, _) = install_hooks(&once, "/other/cockpit").unwrap();
        assert_eq!(events(&moved, "Stop"), ["other.py Stop", "/other/cockpit hook"]);
        let dup = once.replace("other.py Stop", "/bin/cockpit hook");
        let (fixed, _) = install_hooks(&dup, EXE).unwrap();
        assert_eq!(events(&fixed, "Stop"), ["/bin/cockpit hook"]);
    }

    #[test]
    fn uninstall_removes_only_cockpit_hooks() {
        let (once, _) = install_hooks(SETTINGS, EXE).unwrap();
        let (out, lines) = remove_hooks(&once).unwrap();
        assert_eq!(events(&out, "Stop"), ["other.py Stop"]);
        assert!(!out.contains("PostToolUse\""), "events that held only the cockpit hook disappear");
        assert!(out.contains("PostToolUseFailure") && out.contains("\"theme\": \"dark\""));
        assert_eq!(lines.len(), 7);
        assert_eq!(remove_hooks(&out).unwrap().1.len(), 0);
    }

    #[test]
    fn missing_or_broken_settings() {
        let (out, _) = install_hooks("", EXE).unwrap();
        assert_eq!(events(&out, "SessionEnd"), ["/bin/cockpit hook"]);
        assert!(install_hooks("{not json", EXE).is_err());
        assert!(install_hooks("[]", EXE).is_err());
    }

    #[test]
    fn paths_with_spaces_are_quoted_and_still_recognised() {
        let (out, _) = install_hooks("{}", "/a b/cockpit").unwrap();
        assert_eq!(events(&out, "Stop"), ["'/a b/cockpit' hook"]);
        assert!(is_cockpit("'/a b/cockpit' hook"));
        assert!(plist("/a&b/cockpit").contains("/a&amp;b/cockpit"));
    }
}
