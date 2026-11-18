//! The update, reload and rollback guards end to end: temp HOME, temp database, and fake `launchctl` and
//! `cargo` on PATH that record every call. Nothing here can reach the real daemon, binaries or hooks.
use std::path::PathBuf;
use std::process::Command;

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let root = std::env::temp_dir().join(format!("cockpit-updcli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for tool in ["launchctl", "cargo"] {
            let p = root.join("bin").join(tool);
            std::fs::write(&p, format!("#!/bin/sh\necho \"{tool} $@\" >> {}/calls.log\n", root.display())).unwrap();
            std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        Env { root }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_cockpit"))
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("COCKPIT_DB", self.root.join("t.db"))
            .env("COCKPIT_RELEASE_BASE", "http://127.0.0.1:1")
            .env("PATH", format!("{}/bin:/usr/bin:/bin", self.root.display()))
            .env_remove("COCKPIT_ALLOW_REAL_UPDATE")
            .output()
            .unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.root.join("calls.log")).unwrap_or_default()
    }
}

#[test]
fn a_development_build_refuses_update_and_rollback() {
    let e = Env::new("dev");
    for cmd in ["update", "rollback"] {
        let (ok, out) = e.run(&[cmd]);
        assert!(!ok, "{cmd}: {out}");
        assert!(
            out.contains("this is a development build") && out.contains("update applies to the installed cockpit at"),
            "{out}"
        );
    }
    assert_eq!(e.calls(), "", "nothing was run: no cargo install, no daemon restart");
}

#[test]
fn an_instance_with_its_own_database_only_prints_the_reload_steps() {
    let e = Env::new("iso");
    let (ok, out) = e.run(&["reload"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("isolated instance")
            && out.contains("1. restart the daemon")
            && out.contains("2. replace the queue"),
        "{out}"
    );
    assert_eq!(e.calls(), "", "the daemon was not touched");
}

#[test]
fn dry_runs_print_the_steps_of_the_configured_channel() {
    let e = Env::new("dry");
    let (ok, out) = e.run(&["update", "--dry-run"]);
    assert!(ok && out.contains("1. cd ") && out.contains("cargo install --path ."), "{out}");
    let (ok, _) = e.run(&["tool", "settings.set", r#"{"key":"update_channel","value":"release"}"#]);
    assert!(ok);
    let (ok, out) = e.run(&["update", "--dry-run"]);
    assert!(ok, "{out}");
    assert!(
        out.starts_with("1. curl http://127.0.0.1:1/Xunrise/ticket-cockpit/releases/latest/download/manifest.json"),
        "{out}"
    );
    let (ok, out) = e.run(&["rollback", "--dry-run"]);
    assert!(ok && out.contains("1. swap ") && out.contains("cockpit.prev"), "{out}");
    assert_eq!(e.calls(), "");
}
