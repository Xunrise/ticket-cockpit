//! Where the cockpit keeps things. `COCKPIT_DB` moves the database (and the context
//! files next to it), so test and validation databases never touch the real install.
use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn data_dir() -> PathBuf {
    home().join("Library/Application Support/cockpit")
}

pub fn db_path() -> PathBuf {
    std::env::var_os("COCKPIT_DB").map(PathBuf::from).unwrap_or_else(|| data_dir().join("cockpit.db"))
}

/// Saved meeting notes and other linked context files.
pub fn context_dir() -> PathBuf {
    match std::env::var_os("COCKPIT_DB") {
        Some(db) => PathBuf::from(db).with_extension("context"),
        None => data_dir().join("context"),
    }
}

pub fn config_dir() -> PathBuf {
    home().join(".config/cockpit")
}

pub fn tmux_conf() -> PathBuf {
    config_dir().join("cockpit.tmux.conf")
}

pub fn claude_settings() -> PathBuf {
    home().join(".claude/settings.json")
}

pub fn coach_baseline() -> PathBuf {
    home().join(".config/palaver/coach-baseline.json")
}

pub fn palaver_bin() -> PathBuf {
    std::env::var_os("PALAVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Applications/Palaver.app/Contents/Resources/bin/palaver"))
}

pub fn palaver_sock() -> PathBuf {
    home().join("Library/Application Support/palaver/app.sock")
}

pub fn launch_agent_plist() -> PathBuf {
    home().join("Library/LaunchAgents/io.cockpit.daemon.plist")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_db_is_under_application_support() {
        if std::env::var_os("COCKPIT_DB").is_none() {
            assert!(super::db_path().ends_with("Library/Application Support/cockpit/cockpit.db"));
        }
    }
}
