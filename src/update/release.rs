//! The release channel: prebuilt binaries from GitHub Releases. Plain HTTPS through `/usr/bin/curl`
//! (every Mac has it, so the binary carries no TLS stack), verified by sha256, swapped in atomically.
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The release asset target of the running build.
pub fn target() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    }
}

/// Base of the release downloads; `COCKPIT_RELEASE_BASE` points it at a local server (demos, tests).
pub fn base() -> String {
    std::env::var("COCKPIT_RELEASE_BASE")
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "https://github.com".into())
        .trim_end_matches('/')
        .to_string()
}

pub fn manifest_url(base: &str, repo: &str) -> String {
    format!("{base}/{repo}/releases/latest/download/manifest.json")
}

/// `v1.2.3` or `1.2.3` as numbers.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut p = v.trim().trim_start_matches('v').split(['.', '-', '+']).map(|x| x.parse::<u64>());
    Some((p.next()?.ok()?, p.next()?.ok()?, p.next()?.ok()?))
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "300"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("curl: {e}"))?;
    if !out.status.success() {
        bail!("curl {}: {}", args.last().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// The latest release's manifest: `{version, commit, published, notes, assets: {target: {url, sha256, size}}}`.
pub fn fetch_manifest(base: &str, repo: &str) -> Result<Value> {
    let url = manifest_url(base, repo);
    let body = curl(&["--max-time", "20", &url]).map_err(|_| anyhow!("no release manifest at {url}"))?;
    serde_json::from_slice(&body).map_err(|e| anyhow!("bad manifest at {url}: {e}"))
}

/// Newer means a higher version, or the same version built from another commit.
pub fn is_newer(manifest: &Value, version: &str, commit: &str) -> bool {
    let (Some(theirs), Some(mine)) =
        (parse_version(manifest["version"].as_str().unwrap_or("")), parse_version(version))
    else {
        return false;
    };
    theirs > mine
        || (theirs == mine
            && manifest["commit"]
                .as_str()
                .is_some_and(|c| !c.is_empty() && !commit.starts_with(c) && !c.starts_with(commit)))
}

/// Same shape as the local channel's status; `available` is the release tag.
pub fn status(base: &str, repo: &str, version: &str, commit: &str) -> Value {
    let none =
        |s: String| json!({"available": null, "commits_ahead": null, "behind_remote": 0, "summary": s, "notes": ""});
    if repo.is_empty() {
        return none("release_repo is not set".into());
    }
    let m = match fetch_manifest(base, repo) {
        Ok(m) => m,
        Err(e) => return none(e.to_string()),
    };
    let tag = m["version"].as_str().unwrap_or("");
    let (newer, has_asset) = (is_newer(&m, version, commit), m["assets"][target()]["url"].is_string());
    let summary = match (newer, has_asset) {
        (true, true) => format!("update available: {tag}"),
        (true, false) => format!("{tag} has no asset for {}", target()),
        _ => "up to date".into(),
    };
    json!({"available": (newer && has_asset).then(|| tag.to_string()), "commits_ahead": null, "behind_remote": 0,
           "summary": summary, "latest": tag, "notes": m["notes"].as_str().unwrap_or(""), "published": m["published"]})
}

pub fn sha256_of(file: &Path) -> Result<String> {
    let mut f = std::fs::File::open(file)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn verify_sha256(file: &Path, want: &str) -> Result<()> {
    let want = want.trim().to_lowercase();
    let got = sha256_of(file)?;
    if want != got {
        bail!("sha256 mismatch for {}: expected {want}, got {got}", file.display());
    }
    Ok(())
}

/// Downloads the latest release asset for this target into `dir`, verifies and unpacks it; returns the tag and binary.
pub fn stage(base: &str, repo: &str, dir: &Path) -> Result<(String, PathBuf)> {
    let m = fetch_manifest(base, repo)?;
    let tag = m["version"].as_str().ok_or_else(|| anyhow!("manifest has no version"))?.to_string();
    let asset = &m["assets"][target()];
    let (Some(url), Some(sha)) = (asset["url"].as_str(), asset["sha256"].as_str()) else {
        bail!("release {tag} has no asset for {}", target())
    };
    std::fs::create_dir_all(dir)?;
    let archive = dir.join(format!("cockpit-{}.tar.gz", target()));
    curl(&["-o", &archive.to_string_lossy(), url])?;
    verify_sha256(&archive, sha)?;
    let st = Command::new("/usr/bin/tar").arg("-xzf").arg(&archive).arg("-C").arg(dir).status()?;
    let bin = dir.join("cockpit");
    if !st.success() || !bin.exists() {
        bail!("the archive has no `cockpit` binary");
    }
    Ok((tag, bin))
}

pub fn prev_of(exe: &Path) -> PathBuf {
    exe.with_file_name("cockpit.prev")
}

/// Puts `new` in place of `exe` atomically (the rename); the previous binary stays as `cockpit.prev` next to it.
pub fn swap_in(new: &Path, exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = exe.with_file_name("cockpit.new");
    std::fs::copy(new, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    if exe.exists() {
        std::fs::copy(exe, prev_of(exe))?;
    }
    std::fs::rename(&tmp, exe)?;
    Ok(())
}

/// Swaps `cockpit.prev` and the binary, so a second rollback undoes the first.
pub fn rollback(exe: &Path) -> Result<()> {
    let prev = prev_of(exe);
    if !prev.exists() {
        bail!("no {} to roll back to", prev.display());
    }
    let swap = exe.with_file_name("cockpit.swap");
    std::fs::rename(exe, &swap)?;
    std::fs::rename(&prev, exe)?;
    std::fs::rename(&swap, &prev)?;
    Ok(())
}

pub fn steps(base: &str, repo: &str, exe: &Path) -> Vec<String> {
    vec![
        format!("curl {}", manifest_url(base, repo)),
        format!("download the asset for {} from the manifest", target()),
        "verify its sha256 against the manifest".into(),
        "unpack it".into(),
        format!("copy {} to cockpit.prev, then rename the new binary over it", exe.display()),
    ]
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

    /// A throwaway HTTP server on a free port that answers fixed paths with fixed bodies. Stops on drop.
    pub struct Server {
        pub base: String,
        stop: Arc<AtomicBool>,
    }

    impl Server {
        /// `routes` gets the server's base URL, so a manifest can point at the server itself.
        pub fn start(routes: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> Server {
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
                            let _ = write!(
                                c,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = c.write_all(body);
                        }
                        None => {
                            let _ =
                                write!(c, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                    }
                }
            });
            Server { base, stop }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// A fake release of repo `o/r`: a tar.gz holding a `cockpit` script, and the manifest naming it.
    /// `sha` overrides the manifest's checksum (to test a mismatch).
    pub fn fake_release(dir: &Path, version: &str, commit: &str, sha: Option<&str>) -> Server {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("cockpit"), format!("#!/bin/sh\necho fake {version}\n")).unwrap();
        let archive = dir.join("asset.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg("cockpit")
            .status()
            .unwrap()
            .success());
        let real = sha256_of(&archive).unwrap();
        let bytes = std::fs::read(&archive).unwrap();
        Server::start(|base| {
            let manifest = json!({
                "version": version, "commit": commit, "published": "2026-11-18T00:00:00Z", "notes": "- a change",
                "assets": {target(): {"url": format!("{base}/asset.tar.gz"), "sha256": sha.unwrap_or(&real), "size": bytes.len()}},
            });
            vec![
                ("/o/r/releases/latest/download/manifest.json".to_string(), manifest.to_string().into_bytes()),
                ("/asset.tar.gz".to_string(), bytes.clone()),
            ]
        })
    }

    #[test]
    fn versions_compare_as_numbers_and_the_commit_breaks_ties() {
        assert!(parse_version("v0.10.0") > parse_version("0.9.9"));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("nope"), None);
        let m = |v: &str, c: &str| json!({"version": v, "commit": c});
        assert!(is_newer(&m("v0.2.0", "aaa"), "0.1.0", "zzz"));
        assert!(!is_newer(&m("v0.1.0", "aaa"), "0.2.0", "zzz"), "older release");
        assert!(is_newer(&m("v0.2.0", "aaaaaaaaaaaa"), "0.2.0", "bbbbbbbbbbbb"), "same version, other commit");
        assert!(!is_newer(&m("v0.2.0", "aaaaaaaaaaaa"), "0.2.0", "aaaaaaaaaaaa"));
        assert!(!is_newer(&m("v0.2.0", "aaaa"), "0.2.0", "aaaaaaaaaaaa"), "short commit is a prefix");
        assert!(!is_newer(&m("garbage", ""), "0.2.0", "x"));
    }

    #[test]
    fn status_reads_the_manifest_from_a_local_server() {
        let d = Dir::new();
        let srv = fake_release(d.path(), "v0.3.0", "abcabcabcabc", None);
        let s = status(&srv.base, "o/r", "0.2.0", "111111111111");
        assert_eq!((s["available"].as_str(), s["notes"].as_str()), (Some("v0.3.0"), Some("- a change")));
        assert_eq!(s["summary"], "update available: v0.3.0");
        let s = status(&srv.base, "o/r", "0.3.0", "abcabcabcabc");
        assert_eq!((s["available"].is_null(), s["summary"].as_str()), (true, Some("up to date")));
        let s = status(&srv.base, "no/such", "0.2.0", "x");
        assert!(s["summary"].as_str().unwrap().starts_with("no release manifest at"), "{s}");
        assert_eq!(status(&srv.base, "", "0.2.0", "x")["summary"], "release_repo is not set");
    }

    #[test]
    fn a_sha256_mismatch_stops_the_download() {
        let d = Dir::new();
        let srv = fake_release(d.path(), "v0.3.0", "abc", Some(&"0".repeat(64)));
        let e = stage(&srv.base, "o/r", &d.path().join("stage")).unwrap_err().to_string();
        assert!(e.contains("sha256 mismatch") && e.contains("expected 000") && e.contains("got "), "{e}");
        assert!(!d.path().join("stage/cockpit").exists(), "nothing was unpacked");
    }

    #[test]
    fn stage_swap_and_rollback_in_a_temp_dir() {
        let d = Dir::new();
        let srv = fake_release(d.path(), "v0.3.0", "abc", None);
        let (tag, bin) = stage(&srv.base, "o/r", &d.path().join("stage")).unwrap();
        assert_eq!(tag, "v0.3.0");
        assert!(std::fs::read_to_string(&bin).unwrap().contains("fake v0.3.0"));
        let exe = d.path().join("bin/cockpit");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "old").unwrap();
        swap_in(&bin, &exe).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("fake v0.3.0"));
        assert_eq!(std::fs::read_to_string(prev_of(&exe)).unwrap(), "old");
        assert!(!exe.with_file_name("cockpit.new").exists());
        rollback(&exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert!(
            std::fs::read_to_string(prev_of(&exe)).unwrap().contains("fake v0.3.0"),
            "a second rollback undoes the first"
        );
        std::fs::remove_file(prev_of(&exe)).unwrap();
        assert!(rollback(&exe).unwrap_err().to_string().contains("no "));
    }

    #[test]
    fn an_asset_missing_for_this_machine_is_reported() {
        let d = Dir::new();
        let srv = Server::start(|_| {
            vec![(
                "/o/r/releases/latest/download/manifest.json".into(),
                br#"{"version":"v9.0.0","assets":{}}"#.to_vec(),
            )]
        });
        assert!(stage(&srv.base, "o/r", &d.path().join("s")).unwrap_err().to_string().contains("no asset for"));
        assert!(status(&srv.base, "o/r", "0.1.0", "x")["summary"].as_str().unwrap().contains("no asset for"));
    }
}
