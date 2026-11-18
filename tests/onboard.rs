//! `cockpit onboard --yes` end to end on a temp HOME holding fake transcripts and a git repo.
use std::process::Command;

#[test]
fn onboard_yes_creates_a_project_per_repo_and_is_rerunnable() {
    let root = std::env::temp_dir().join(format!("cockpit-onboard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, repo) = (root.join("home"), root.join("home/work/demo"));
    std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap().success());
    let dir = home.join(".claude/projects/-work-demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("s.jsonl"), format!("{{\"cwd\":\"{}\",\"message\":\"hi\"}}\n", repo.display())).unwrap();

    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_cockpit"))
            .args(args)
            .env("HOME", &home)
            .env("COCKPIT_DB", root.join("t.db"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let out = run(&["onboard", "--yes"]);
    assert!(out.contains("1 sessions") && out.contains("Created: demo"), "{out}");
    assert!(run(&["tool", "project.list"]).contains("\"name\": \"demo\""));
    assert!(run(&["onboard", "--yes"]).contains("Nothing new to create"));

    // without --yes and without a terminal it refuses instead of hanging
    let o = Command::new(env!("CARGO_BIN_EXE_cockpit"))
        .arg("onboard")
        .env("HOME", &home)
        .env("COCKPIT_DB", root.join("t.db"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("--yes"));
    let _ = std::fs::remove_dir_all(&root);
}
