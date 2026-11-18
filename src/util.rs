//! Small helpers: ISO timestamps (the format the prototype stored), shell-outs.
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Epoch milliseconds as `2026-11-18T10:20:30.123Z`.
pub fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", t / 3600, t % 3600 / 60, t % 60, ms.rem_euclid(1000))
}

pub fn now_iso() -> String {
    iso_from_ms(now_ms())
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff]Z` (extra fraction digits ignored).
pub fn ms_from_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(n(0..4)?, n(5..7)? as u32, n(8..10)? as u32);
    let mut ms = (days * 86_400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?) * 1000;
    if b.get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
        ms += format!("{frac:0<3}").parse::<i64>().ok()?;
    }
    Some(ms)
}

/// Runs a command, returns trimmed stdout on success, empty string otherwise.
pub fn sh(prog: &str, args: &[&str], cwd: Option<&Path>) -> String {
    let mut c = Command::new(prog);
    c.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    match c.output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

pub fn git(cwd: &str, args: &[&str]) -> String {
    sh("git", args, Some(Path::new(cwd)))
}

/// Minimal HTTP/1.0 GET against a local `host:port`; returns the body of a 200 response.
pub fn http_get(addr: &str, path: &str, timeout: std::time::Duration) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    let mut s = addr.to_socket_addrs().ok()?.find_map(|a| std::net::TcpStream::connect_timeout(&a, timeout).ok())?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    // one write: a server answering after its first read must see the whole request
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.0 200")
        .then(|| body.to_string())
        .or_else(|| head.starts_with("HTTP/1.1 200").then(|| body.to_string()))
}

pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_roundtrip() {
        let ms = 1_795_000_123_456;
        assert_eq!(ms_from_iso(&iso_from_ms(ms)), Some(ms));
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(ms_from_iso("2026-11-18T00:00:00Z"), ms_from_iso("2026-11-18T00:00:00.000Z"));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn http_get_reads_a_200_body_and_rejects_others() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for (i, s) in l.incoming().flatten().enumerate() {
                let mut s = s;
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let (status, body) = if i == 0 { ("200 OK", "hello") } else { ("404 Not Found", "no") };
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            }
        });
        let t = std::time::Duration::from_secs(2);
        assert_eq!(http_get(&addr, "/x", t).as_deref(), Some("hello"));
        assert_eq!(http_get(&addr, "/x", t), None);
        assert_eq!(http_get("127.0.0.1:1", "/x", t), None);
    }
}
