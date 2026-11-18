//! `install.sh` against a local fake release (a local HTTP server, a temp HOME): install, PATH warning,
//! channel set, previous binary kept, and a checksum mismatch that installs nothing.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Server {
    base: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(routes: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let routes = routes(&base);
    l.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let Ok((mut c, _)) = l.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };
            c.set_nonblocking(false).unwrap();
            let mut buf = [0u8; 2048];
            let n = c.read(&mut buf).unwrap_or(0);
            let path = String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1).unwrap_or("").to_string();
            match routes.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = c.write_all(body);
                }
                None => {
                    let _ = write!(c, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }
        }
    });
    Server { base, stop }
}

fn target() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    }
}

fn sha256(file: &Path) -> String {
    let out = Command::new("shasum").args(["-a", "256"]).arg(file).output().unwrap();
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string()
}

struct Rig {
    root: PathBuf,
    server: Server,
}

/// A fake release whose `cockpit` is a script that records its arguments in `$HOME/calls.log`.
fn rig(tag: &str, sha_override: Option<&str>) -> Rig {
    let root = std::env::temp_dir().join(format!("cockpit-installsh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (src, home) = (root.join("src"), root.join("home"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(src.join("cockpit"), "#!/bin/sh\necho \"$@\" >> \"$HOME/calls.log\"\n").unwrap();
    let archive = root.join("asset.tar.gz");
    assert!(Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&src)
        .arg("cockpit")
        .status()
        .unwrap()
        .success());
    let (bytes, sha) = (std::fs::read(&archive).unwrap(), sha256(&archive));
    let server = serve(|base| {
        let manifest = serde_json::json!({
            "version": "0.9.0", "commit": "abc", "published": "now", "notes": "",
            "assets": {target(): {"url": format!("{base}/asset.tar.gz"), "sha256": sha_override.unwrap_or(&sha), "size": bytes.len()}},
        });
        vec![
            ("/o/r/releases/latest/download/manifest.json".into(), manifest.to_string().into_bytes()),
            ("/asset.tar.gz".into(), bytes.clone()),
        ]
    });
    Rig { root, server }
}

impl Rig {
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn run(&self) -> (bool, String) {
        let out = Command::new("sh")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
            .env("HOME", self.home())
            .env("PATH", "/usr/bin:/bin")
            .env("COCKPIT_RELEASE_BASE", &self.server.base)
            .env("COCKPIT_RELEASE_REPO", "o/r")
            .env_remove("COCKPIT_INSTALL_DIR")
            .output()
            .unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.home().join("calls.log")).unwrap_or_default()
    }
}

#[test]
fn installs_the_release_sets_the_channel_and_registers() {
    let r = rig("ok", None);
    let (ok, out) = r.run();
    assert!(ok, "{out}");
    assert!(out.contains("downloading cockpit 0.9.0") && out.contains("installed "), "{out}");
    assert!(r.home().join(".local/bin/cockpit").exists());
    assert!(out.contains("is not on your PATH"), "{out}");
    let calls = r.calls();
    assert!(calls.contains(r#"tool settings.set {"key":"update_channel","value":"release"}"#), "{calls}");
    assert!(calls.lines().last() == Some("install"), "cockpit install runs last: {calls}");
    assert!(!r.home().join(".local/bin/cockpit.prev").exists(), "nothing to keep on a first install");

    // a second run keeps the previous binary
    let (ok, out) = r.run();
    assert!(ok, "{out}");
    assert!(r.home().join(".local/bin/cockpit.prev").exists());
}

#[test]
fn no_path_warning_when_the_directory_is_on_the_path() {
    let r = rig("path", None);
    let out = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("HOME", r.home())
        .env("PATH", format!("{}/.local/bin:/usr/bin:/bin", r.home().display()))
        .env("COCKPIT_RELEASE_BASE", &r.server.base)
        .env("COCKPIT_RELEASE_REPO", "o/r")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && !text.contains("not on your PATH"), "{text}");
}

#[test]
fn a_sha256_mismatch_fails_and_installs_nothing() {
    let r = rig("bad", Some(&"0".repeat(64)));
    let (ok, out) = r.run();
    assert!(!ok, "{out}");
    assert!(out.contains("sha256 mismatch: expected 000"), "{out}");
    assert!(!r.home().join(".local/bin/cockpit").exists());
    assert_eq!(r.calls(), "");
}
