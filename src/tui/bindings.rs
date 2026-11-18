//! The queue's tmux key bindings, re-asserted every tick from the settings: the last queue started owns
//! them, and a restarted queue takes them back.
use crate::tmux::{Server, Tmux};
use serde_json::Value;

const CK: Server = Server::Cockpit;
pub const DEFAULT_QUEUE_KEY: &str = "S-Escape";
/// Keys earlier prototypes bound; removed when the queue starts.
pub const LEGACY_KEYS: [&str; 3] = ["M-q", "M-Left", "C-]"];

/// Esc on a working Claude asks first. `i` interrupts, `k` keeps it running, `n` interrupts and turns the
/// confirmation off for good (the tick persists the tmux option into the setting).
pub const ESC_MENU: &str = "display-menu -T '#[align=centre]Claude is working' -x C -y C 'Interrupt the running task' i 'send-keys Escape' 'Keep it running' k '' '' 'Interrupt, never ask again' n 'send-keys Escape ; set -g @tk_confirm_esc 0'";
pub const ESC_COND: &str = "#{&&:#{==:#{@tk_state},working},#{@tk_confirm_esc}}";

/// What to do with the confirmation option after reading both sides.
#[derive(Debug, PartialEq)]
pub enum Confirm {
    /// The menu's "never ask again" flipped the tmux option to 0 while the setting is still on: persist it.
    PersistOff,
    /// Mirror the setting into tmux.
    Mirror(bool),
}

pub fn confirm_sync(setting_on: bool, tmux_value: &str) -> Confirm {
    if setting_on && tmux_value == "0" {
        Confirm::PersistOff
    } else {
        Confirm::Mirror(setting_on)
    }
}

pub fn queue_key(settings: &[Value]) -> String {
    settings
        .iter()
        .find(|s| s["key"] == "queue_key")
        .and_then(|s| s["value"].as_str())
        .filter(|k| !k.is_empty())
        .unwrap_or(DEFAULT_QUEUE_KEY)
        .to_string()
}

pub fn confirm_setting(settings: &[Value]) -> bool {
    settings.iter().find(|s| s["key"] == "confirm_interrupt").map(|s| s["value"] != Value::Bool(false)).unwrap_or(true)
}

pub fn remove_legacy(tmux: &Tmux) {
    for old in LEGACY_KEYS {
        let _ = tmux.run(CK, &["unbind", "-n", old]);
    }
}

/// Binds the queue key and Esc. Returns the key now bound, so a changed setting unbinds the previous one.
pub fn assert_keys(tmux: &Tmux, me: &str, key: &str, bound: &str) -> String {
    if !bound.is_empty() && bound != key {
        let _ = tmux.run(CK, &["unbind", "-n", bound]);
    }
    let _ = tmux.run(CK, &["bind", "-n", key, &format!("select-window -t {me} ; select-pane -t {me}")]);
    let _ = tmux.run(CK, &["bind", "-n", "Escape", "if", "-F", ESC_COND, ESC_MENU, "send-keys Escape"]);
    key.to_string()
}

/// Keeps `@tk_confirm_esc` in step with the setting. Returns true when the setting must be switched off.
pub fn sync_confirm(tmux: &Tmux, setting_on: bool) -> bool {
    match confirm_sync(setting_on, &tmux.quiet(CK, &["show", "-gv", "@tk_confirm_esc"])) {
        Confirm::PersistOff => true,
        Confirm::Mirror(on) => {
            let _ = tmux.run(CK, &["set", "-g", "@tk_confirm_esc", if on { "1" } else { "0" }]);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::testutil;
    use serde_json::json;

    #[test]
    fn never_ask_again_is_persisted_otherwise_the_setting_wins() {
        assert_eq!(confirm_sync(true, "0"), Confirm::PersistOff);
        assert_eq!(confirm_sync(true, "1"), Confirm::Mirror(true));
        assert_eq!(confirm_sync(true, ""), Confirm::Mirror(true));
        assert_eq!(confirm_sync(false, "0"), Confirm::Mirror(false));
        assert_eq!(confirm_sync(false, "1"), Confirm::Mirror(false));
    }

    #[test]
    fn settings_give_the_keys() {
        assert_eq!(queue_key(&[]), "S-Escape");
        assert_eq!(queue_key(&[json!({"key": "queue_key", "value": "F12"})]), "F12");
        assert!(confirm_setting(&[]));
        assert!(!confirm_setting(&[json!({"key": "confirm_interrupt", "value": false})]));
    }

    #[test]
    fn keys_are_bound_and_rebound_on_a_private_server() {
        let Some(g) = testutil::guard() else { return };
        let t = &g.0;
        t.run(CK, &["new-session", "-d", "-s", "cockpit", "-x", "80", "-y", "24", "cat"]).unwrap();
        let me = t.run(CK, &["list-panes", "-t", "cockpit", "-F", "#{pane_id}"]).unwrap();
        let bound = assert_keys(t, &me, "S-Escape", "");
        let keys = t.run(CK, &["list-keys", "-T", "root"]).unwrap();
        assert!(keys.contains("S-Escape") && keys.contains("Claude is working"), "{keys}");
        let bound = assert_keys(t, &me, "F12", &bound);
        assert_eq!(bound, "F12");
        let keys = t.run(CK, &["list-keys", "-T", "root"]).unwrap();
        assert!(!keys.contains("S-Escape") && keys.contains("F12"));
        assert!(!sync_confirm(t, true));
        assert_eq!(t.quiet(CK, &["show", "-gv", "@tk_confirm_esc"]), "1");
        t.run(CK, &["set", "-g", "@tk_confirm_esc", "0"]).unwrap();
        assert!(sync_confirm(t, true));
    }
}
