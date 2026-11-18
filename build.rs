// Embeds the commit the binary is built from (COCKPIT_GIT_SHA at runtime overrides it).
fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=COCKPIT_GIT_SHA={sha}");
    println!("cargo:rerun-if-changed=build.rs");
    for what in ["HEAD", "refs"] {
        if let Ok(o) = std::process::Command::new("git").args(["rev-parse", "--git-path", what]).output() {
            println!("cargo:rerun-if-changed={}", String::from_utf8_lossy(&o.stdout).trim());
        }
    }
}
