//! `cockpit install` / `uninstall` end to end on a temp HOME with a fake `launchctl` on PATH.
use std::path::{Path, PathBuf};
use std::process::Command;

struct Env {
    home: PathBuf,
    bin: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let root = std::env::temp_dir().join(format!("cockpit-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, bin) = (root.join("home"), root.join("bin"));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        // `print` succeeds once a bootstrap happened and until a bootout
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> {0}/launchctl.log\ncase \"$1\" in\n  bootstrap) touch {0}/loaded;;\n  bootout) rm -f {0}/loaded;;\n  print) test -f {0}/loaded;;\nesac\n",
            root.display()
        );
        let p = bin.join("launchctl");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        Env { home, bin }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_cockpit"))
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env_remove("COCKPIT_DB")
            .output()
            .unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    }

    fn launchctl_log(&self) -> String {
        std::fs::read_to_string(self.bin.parent().unwrap().join("launchctl.log")).unwrap_or_default()
    }

    fn settings(&self) -> PathBuf {
        self.home.join(".claude/settings.json")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.bin.parent().unwrap());
    }
}

const SETTINGS: &str = "{\n  \"theme\": \"dark\",\n  \"hooks\": {\n    \"Stop\": [\n      {\"hooks\": [{\"type\": \"command\", \"command\": \"keep.sh\"}]},\n      {\"hooks\": [{\"type\": \"command\", \"command\": \"node /p/ticket-cockpit-proto/tk.mjs hook\", \"timeout\": 5}]}\n    ]\n  }\n}\n";

fn backups(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("cockpit-backup"))
        .count()
}

#[test]
fn install_is_idempotent_and_uninstall_reverses_it() {
    let e = Env::new("full");
    std::fs::write(e.settings(), SETTINGS).unwrap();
    let exe = env!("CARGO_BIN_EXE_cockpit");

    let (ok, out) = e.run(&["install"]);
    assert!(ok, "{out}");
    assert!(out.contains("tmux config") && out.contains("created"), "{out}");
    let conf = e.home.join(".config/cockpit/cockpit.tmux.conf");
    assert!(std::fs::read_to_string(conf).unwrap().contains("status off"));
    let plist = std::fs::read_to_string(e.home.join("Library/LaunchAgents/io.cockpit.daemon.plist")).unwrap();
    assert!(plist.contains(&format!("<string>{exe}</string>")) && plist.contains("<key>KeepAlive</key>"));
    assert!(plist.contains(&format!("{}/Library/Application Support/cockpit/logs/daemon.log", e.home.display())));
    assert!(e.launchctl_log().contains("bootstrap gui/") && e.launchctl_log().contains("kickstart gui/"));
    let s = std::fs::read_to_string(e.settings()).unwrap();
    assert!(!s.contains("tk.mjs") && s.contains("keep.sh") && s.contains("\"theme\": \"dark\""));
    assert_eq!(s.matches(&format!("{exe} hook")).count(), 7);
    assert_eq!(backups(e.settings().parent().unwrap()), 1);

    // second run: nothing changes, no new backup, no new launchctl mutation
    let log = e.launchctl_log();
    let (ok, out) = e.run(&["install"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(e.settings()).unwrap(), s);
    assert_eq!(backups(e.settings().parent().unwrap()), 1);
    assert!(out.contains("already loaded") && out.contains("Claude settings") && out.contains("unchanged"), "{out}");
    assert_eq!(e.launchctl_log().matches("bootstrap").count(), log.matches("bootstrap").count());

    let (ok, out) = e.run(&["uninstall"]);
    assert!(ok, "{out}");
    assert!(e.launchctl_log().contains("bootout gui/"));
    assert!(!e.home.join("Library/LaunchAgents/io.cockpit.daemon.plist").exists());
    let s = std::fs::read_to_string(e.settings()).unwrap();
    assert!(!s.contains("cockpit hook") && s.contains("keep.sh"));
    assert!(e.home.join(".config/cockpit/cockpit.tmux.conf").exists(), "config is kept");
}

#[test]
fn dry_run_writes_nothing_and_never_mutates_launchd() {
    let e = Env::new("dry");
    std::fs::write(e.settings(), SETTINGS).unwrap();
    let (ok, out) = e.run(&["install", "--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("(dry-run) tmux config") && out.contains("would be created"), "{out}");
    assert!(out.contains("hooks.Stop: replaced 1 prototype") && out.contains("launchctl bootstrap"), "{out}");
    assert_eq!(std::fs::read_to_string(e.settings()).unwrap(), SETTINGS);
    assert!(!e.home.join(".config").exists() && !e.home.join("Library").exists());
    assert!(!e.launchctl_log().contains("bootstrap"));
    assert_eq!(backups(e.settings().parent().unwrap()), 0);
    let (ok, out) = e.run(&["uninstall", "--dry-run"]);
    assert!(ok, "{out}");
}

#[test]
fn unusable_settings_are_left_alone() {
    let e = Env::new("bad");
    std::fs::write(e.settings(), "{ nope").unwrap();
    let (ok, out) = e.run(&["install"]);
    assert!(!ok && out.contains("left untouched"), "{out}");
    assert_eq!(std::fs::read_to_string(e.settings()).unwrap(), "{ nope");
}
