//! `cockpit onboard`: seeds a fresh install from how the user already works. Scans recent Claude
//! transcripts for working directories (only the `cwd` fields are read), proposes one project per
//! repo, links projects to Palaver and offers an onboarding Claude session that proposes tickets.
use crate::cli::switch;
use crate::ctx::Ctx;
use crate::{palaver, panes, projects, tickets, tui};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DAYS: u64 = 60;
pub const WINDOW: &str = "onboarding";

/// A repo found in the transcripts.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub path: String,
    pub sessions: usize,
    /// Transcript directories (under ~/.claude/projects) that hold its sessions.
    pub transcript_dirs: BTreeSet<PathBuf>,
}

/// The `cwd` values of one transcript. Only that field is looked at, never the message content.
pub fn cwds_in(reader: impl BufRead) -> BTreeSet<String> {
    const KEY: &str = "\"cwd\":";
    let mut out = BTreeSet::new();
    for line in reader.lines().map_while(|l| l.ok()) {
        if let Some(i) = line.find(KEY) {
            if let Some(Ok(cwd)) =
                serde_json::Deserializer::from_str(line[i + KEY.len()..].trim_start()).into_iter::<String>().next()
            {
                out.insert(cwd);
            }
        }
    }
    out
}

/// Repos behind the transcripts modified within `days`, most sessions first. Worktrees count for their main repo;
/// directories that are gone or not in a git repo are skipped.
pub fn scan(claude_projects: &Path, days: u64) -> Vec<Found> {
    let oldest = SystemTime::now() - Duration::from_secs(days * 86_400);
    let mut resolved: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    let Ok(dirs) = std::fs::read_dir(claude_projects) else { return vec![] };
    for dir in dirs.flatten().filter(|d| d.path().is_dir()) {
        for f in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let p = f.path();
            let recent = f.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= oldest);
            if p.extension().is_none_or(|e| e != "jsonl") || !recent {
                continue;
            }
            let Ok(file) = std::fs::File::open(&p) else { continue };
            let repos: BTreeSet<String> = cwds_in(std::io::BufReader::new(file))
                .into_iter()
                .filter_map(|cwd| resolved.entry(cwd.clone()).or_insert_with(|| tickets::main_checkout(&cwd)).clone())
                .collect();
            for path in repos {
                let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let e = found.entry(path.clone()).or_insert_with(|| Found {
                    name,
                    path,
                    sessions: 0,
                    transcript_dirs: BTreeSet::new(),
                });
                e.sessions += 1;
                e.transcript_dirs.insert(dir.path());
            }
        }
    }
    let mut v: Vec<Found> = found.into_values().collect();
    v.sort_by(|a, b| b.sessions.cmp(&a.sessions).then(a.name.cmp(&b.name)));
    v
}

/// One project to create from one or more repos.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub name: String,
    pub repos: Vec<Found>,
}

impl Proposal {
    fn sessions(&self) -> usize {
        self.repos.iter().map(|r| r.sessions).sum()
    }
}

pub fn propose(repos: &[Found]) -> Vec<Proposal> {
    repos.iter().map(|r| Proposal { name: r.name.clone(), repos: vec![r.clone()] }).collect()
}

pub struct Io<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
}

impl Io<'_> {
    fn say(&mut self, s: impl AsRef<str>) {
        let _ = writeln!(self.out, "{}", s.as_ref());
    }

    /// The answer to `prompt`; empty input and end of input give `default`.
    fn ask(&mut self, prompt: &str, default: &str) -> String {
        let _ = write!(self.out, "{prompt} ");
        let _ = self.out.flush();
        let mut line = String::new();
        let _ = self.input.read_line(&mut line);
        match line.trim() {
            "" => default.to_string(),
            s => s.to_string(),
        }
    }

    fn yes(&mut self, prompt: &str, default: bool) -> bool {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        let a = self.ask(&format!("{prompt} {hint}"), if default { "y" } else { "n" });
        matches!(a.to_lowercase().as_str(), "y" | "yes")
    }
}

fn show(io: &mut Io, props: &[Proposal]) {
    for (i, p) in props.iter().enumerate() {
        let repos: Vec<&str> = p.repos.iter().map(|r| r.name.as_str()).collect();
        let extra =
            if p.repos.len() > 1 || repos[0] != p.name { format!("  <- {}", repos.join(", ")) } else { String::new() };
        io.say(format!("  {:>2}. {:<28} {:>3} sessions{extra}", i + 1, p.name, p.sessions()));
    }
}

fn numbers(s: &str, max: usize) -> Option<Vec<usize>> {
    let v: Option<Vec<usize>> =
        s.split(',').map(|n| n.trim().parse::<usize>().ok().filter(|n| (1..=max).contains(n))).collect();
    v.filter(|v| !v.is_empty())
}

/// Lets the user rename, merge (comma list) and skip proposals until they accept with Enter.
pub fn review(io: &mut Io, props: &mut Vec<Proposal>) {
    io.say("Commands: Enter accepts | rename N name | merge 1,3,4 [name] | skip N | list");
    loop {
        let line = io.ask(">", "");
        let (cmd, rest) = line.split_once(' ').map_or((line.as_str(), ""), |(c, r)| (c, r.trim()));
        match cmd {
            "" | "ok" | "done" => return,
            "list" => show(io, props),
            "rename" => {
                match rest.split_once(' ').and_then(|(n, name)| Some((numbers(n, props.len())?, name.trim()))) {
                    Some((n, name)) if n.len() == 1 && !name.is_empty() => props[n[0] - 1].name = name.to_string(),
                    _ => io.say("usage: rename N new-name"),
                }
            }
            "skip" => match numbers(rest, props.len()) {
                Some(n) => {
                    let gone: BTreeSet<usize> = n.into_iter().collect();
                    let mut i = 0;
                    props.retain(|_| {
                        i += 1;
                        !gone.contains(&i)
                    });
                }
                None => io.say("usage: skip N[,N...]"),
            },
            "merge" => {
                let (list, name) = rest.split_once(' ').map_or((rest, ""), |(l, n)| (l, n.trim()));
                match numbers(list, props.len()) {
                    Some(n) if n.len() > 1 => {
                        let set: BTreeSet<usize> = n.iter().copied().collect();
                        let first = *set.iter().next().unwrap();
                        let mut merged = props[first - 1].clone();
                        if !name.is_empty() {
                            merged.name = name.to_string();
                        }
                        for i in set.iter().skip(1) {
                            merged.repos.extend(props[i - 1].repos.clone());
                        }
                        let mut i = 0;
                        props.retain(|_| {
                            i += 1;
                            !set.contains(&i) || i == first
                        });
                        props[first - 1] = merged;
                    }
                    _ => io.say("usage: merge 1,3,4 [name]"),
                }
            }
            _ => io.say("unknown command"),
        }
        show(io, props);
    }
}

/// Creates the projects (and remembers their repos). Returns the names created.
pub fn create_projects(conn: &Connection, props: &[Proposal]) -> Result<Vec<String>> {
    let mut created = vec![];
    for p in props {
        for r in &p.repos {
            tickets::remember_repo(conn, &r.path)?;
        }
        if !projects::exists(conn, &p.name)? {
            tickets::ensure_project(conn, &p.name)?;
            created.push(p.name.clone());
        }
    }
    Ok(created)
}

/// Offers each project a Palaver link; the default is a project of the same name (any case).
fn link_palaver(io: &mut Io, conn: &Connection, bin: &Path, names: &[String], yes: bool) {
    if !bin.exists() {
        io.say("Palaver: not available here, skipping the project links.");
        return;
    }
    let list = match palaver::projects(bin) {
        Ok(l) if !l.is_empty() => l,
        Ok(_) => return io.say("Palaver: no projects, skipping the links."),
        Err(e) => return io.say(format!("Palaver: {e}; skipping the project links.")),
    };
    let pv: Vec<String> = list.iter().filter_map(|p| p["name"].as_str().map(String::from)).collect();
    if !yes {
        io.say("Palaver projects:");
        for (i, n) in pv.iter().enumerate() {
            io.say(format!("  {:>2}. {n}", i + 1));
        }
    }
    for name in names {
        if projects::palaver_of(conn, name).ok().flatten().is_some() {
            continue;
        }
        let guess = pv.iter().find(|n| n.eq_ignore_ascii_case(name)).cloned().unwrap_or_default();
        let pick = if yes {
            guess
        } else {
            let hint = if guess.is_empty() { "none".to_string() } else { guess.clone() };
            match io.ask(&format!("Palaver project for {name} (number or name, - for none) [{hint}]:"), &guess).as_str()
            {
                "-" => String::new(),
                a => a
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| pv.get(n.wrapping_sub(1)).cloned())
                    .unwrap_or_else(|| a.to_string()),
            }
        };
        if pick.is_empty() {
            continue;
        }
        if !pv.contains(&pick) {
            io.say(format!("  no Palaver project {pick}, {name} stays unlinked"));
        } else if projects::link_palaver(conn, name, &pick).is_ok() {
            io.say(format!("  {name} -> Palaver {pick}"));
        }
    }
}

/// The brief for the onboarding Claude session.
pub fn session_prompt(props: &[Proposal], exe: &str) -> String {
    let mut s = format!(
        "You are onboarding the cockpit from how I already work. These are my projects (cockpit project: repos) and where Claude kept the transcripts of the last {DAYS} days (JSONL files, newest by modification time):\n\n"
    );
    for p in props {
        s += &format!("- {}\n", p.name);
        for r in &p.repos {
            let dirs: Vec<String> = r.transcript_dirs.iter().map(|d| d.display().to_string()).collect();
            s += &format!("    repo {} ({}), transcripts in {}\n", r.name, r.path, dirs.join(", "));
        }
    }
    s += &format!(
        "\nFor each project, read the recent transcripts (my messages and your answers, newest first; skim, do not dump) and collect:\n\
         1. Unfinished work: open questions, things postponed (\"later\", \"next\", \"TODO\"), stories or features that were mentioned but not done. Skip what is clearly finished.\n\
         2. Where dev servers are started (npm run dev, cargo run, python -m http.server, ...) with the directory, as default service panes.\n\n\
         Do not file anything yet. Show me, per project, the tickets you propose (short title and one line each) and the service panes, and ask what to keep. Only after I confirm in this session, file them:\n\
         {exe} tool ticket.add '{{\"project\":\"<project>\",\"repo\":\"<repo name>\",\"title\":\"...\",\"body\":\"...\"}}'\n\
         {exe} tool project.set_services '{{\"project\":\"<project>\",\"services\":[{{\"name\":\"web\",\"cmd\":\"...\",\"cwd\":\"...\"}}]}}'\n\
         Keep titles short and do not file duplicates."
    );
    s
}

fn start_session(io: &mut Io, ctx: &Ctx, conn: &Connection, props: &[Proposal]) -> Result<()> {
    let top = props.iter().flat_map(|p| &p.repos).max_by_key(|r| r.sessions).map(|r| r.name.clone());
    let Some(repo) = top else { return Ok(()) };
    tui::write_tmux_conf()?;
    let tmux = crate::tmux::Tmux::system();
    if panes::window_exists(&tmux, WINDOW) {
        io.say(format!("The window {WINDOW} already exists in the cockpit; attach with `cockpit`."));
        return Ok(());
    }
    panes::run_claude(conn, &tmux, &repo, WINDOW, &session_prompt(props, &ctx.exe))?;
    io.say(format!(
        "Started the Claude session in the cockpit window {WINDOW}. Open `cockpit` and pick it from the queue."
    ));
    Ok(())
}

pub struct Options {
    pub yes: bool,
    pub start_session: bool,
    pub days: u64,
}

/// The whole flow against the given streams, so tests can drive it.
pub fn run(io: &mut Io, ctx: &Ctx, claude_projects: &Path, opts: &Options) -> Result<()> {
    let repos = scan(claude_projects, opts.days);
    if repos.is_empty() {
        io.say(format!("No Claude sessions in the last {} days under {}.", opts.days, claude_projects.display()));
        return Ok(());
    }
    io.say(format!("Repos with Claude sessions in the last {} days:", opts.days));
    for r in &repos {
        io.say(format!("  {:>3} sessions  {:<24} {}", r.sessions, r.name, r.path));
    }
    let conn = ctx.db()?;
    let mut props = propose(&repos);
    let before = props.len();
    props.retain(|p| !projects::exists(&conn, &p.name).unwrap_or(false));
    if props.len() < before {
        io.say(format!("{} already have a project and are left alone.", before - props.len()));
    }
    if props.is_empty() {
        io.say("Nothing new to create.");
    } else {
        io.say("\nProposed projects, one per repo:");
        show(io, &props);
        if !opts.yes {
            review(io, &mut props);
        }
        if props.is_empty() || !(opts.yes || io.yes(&format!("Create {} project(s)?", props.len()), true)) {
            io.say("No projects created.");
            return Ok(());
        }
        let created = create_projects(&conn, &props)?;
        io.say(format!("Created: {}", created.join(", ")));
    }
    let names: Vec<String> = props.iter().map(|p| p.name.clone()).collect();
    link_palaver(io, &conn, &ctx.palaver_bin, &names, opts.yes);
    if !props.is_empty()
        && (opts.start_session
            || (!opts.yes
                && io.yes(
                    "Start the onboarding Claude session in the cockpit's tmux (proposes tickets and service panes)?",
                    true,
                )))
    {
        start_session(io, ctx, &conn, &props)?;
    }
    Ok(())
}

/// `cockpit onboard [--yes] [--start-session] [--days N]`.
pub fn command(mut args: Vec<String>) -> Result<()> {
    args.remove(0);
    let yes = switch(&mut args, "yes");
    let start_session = switch(&mut args, "start-session");
    let days = crate::cli::flag(&mut args, "days").map(|d| d.parse()).transpose()?.unwrap_or(DAYS);
    if let Some(a) = args.first() {
        bail!("unexpected argument {a}");
    }
    if !yes && !std::io::stdin().is_terminal() {
        bail!("onboard asks questions and needs a terminal; use --yes to accept the defaults");
    }
    let claude_projects = crate::paths::home().join(".claude/projects");
    let (stdin, mut stdout) = (std::io::stdin(), std::io::stdout());
    run(
        &mut Io { input: &mut stdin.lock(), out: &mut stdout },
        &Ctx::from_env(),
        &claude_projects,
        &Options { yes, start_session, days },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tempdir::Dir;
    use std::io::Cursor;
    use std::process::Command;

    fn git_init(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "i"],
        ] {
            assert!(Command::new("git").args(args).current_dir(path).status().unwrap().success());
        }
    }

    fn transcript(dir: &Path, name: &str, cwds: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let lines: Vec<String> = cwds
            .iter()
            .map(|c| {
                format!(
                    r#"{{"type":"user","cwd":{},"message":{{"content":"say \"cwd\":\"/etc\" please"}}}}"#,
                    serde_json::to_string(c).unwrap()
                )
            })
            .collect();
        std::fs::write(dir.join(name), lines.join("\n") + "\n").unwrap();
    }

    /// repos alpha (with a worktree) and beta, a non-git dir and a vanished one.
    fn fixture(d: &Dir) -> PathBuf {
        let (alpha, beta, plain) =
            (d.path().join("work/alpha"), d.path().join("work/beta"), d.path().join("work/plain"));
        git_init(&alpha);
        git_init(&beta);
        std::fs::create_dir_all(&plain).unwrap();
        let wt = d.path().join("work/alpha-feature");
        assert!(Command::new("git")
            .args(["worktree", "add", "-q", "-b", "f"])
            .arg(&wt)
            .current_dir(&alpha)
            .status()
            .unwrap()
            .success());
        let cp = d.path().join("claude-projects");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        transcript(&cp.join("-alpha"), "1.jsonl", &[&s(&alpha)]);
        transcript(&cp.join("-alpha-feature"), "2.jsonl", &[&s(&wt), &s(&alpha)]);
        transcript(&cp.join("-alpha"), "3.jsonl", &[&s(&alpha.join("src-missing"))]);
        transcript(&cp.join("-beta"), "4.jsonl", &[&s(&beta), &s(&plain), "/does/not/exist"]);
        transcript(&cp.join("-plain"), "5.jsonl", &[&s(&plain)]);
        let old = cp.join("-beta/old.jsonl");
        transcript(&cp.join("-beta"), "old.jsonl", &[&s(&beta)]);
        let f = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(90 * 86_400)).unwrap();
        cp
    }

    #[test]
    fn only_the_cwd_field_is_read() {
        let t =
            "{\"cwd\":\"/a b\",\"message\":\"x \\\"cwd\\\":\\\"/no\\\"\"}\nnot json\n{\"cwd\": \"/c\"}\n{\"no\":1}\n";
        assert_eq!(cwds_in(Cursor::new(t)), BTreeSet::from(["/a b".to_string(), "/c".to_string()]));
    }

    #[test]
    fn scan_resolves_worktrees_skips_non_git_and_old_sessions() {
        let d = Dir::new();
        let cp = fixture(&d);
        let found = scan(&cp, 60);
        let summary: Vec<(&str, usize)> = found.iter().map(|f| (f.name.as_str(), f.sessions)).collect();
        assert_eq!(summary, [("alpha", 2), ("beta", 1)]);
        assert!(found[0].path.ends_with("work/alpha"));
        assert_eq!(found[0].transcript_dirs.len(), 2);
        assert_eq!(scan(&cp, 120).iter().find(|f| f.name == "beta").unwrap().sessions, 2);
    }

    fn proposals() -> Vec<Proposal> {
        ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let f = Found {
                    name: n.to_string(),
                    path: format!("/r/{n}"),
                    sessions: i + 1,
                    transcript_dirs: BTreeSet::new(),
                };
                Proposal { name: n.to_string(), repos: vec![f] }
            })
            .collect()
    }

    fn drive(input: &str, props: &mut Vec<Proposal>) -> String {
        let mut out = Vec::new();
        review(&mut Io { input: &mut Cursor::new(input.to_string()), out: &mut out }, props);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn review_renames_merges_and_skips() {
        let mut p = proposals();
        drive("rename 1 Alpha\nskip 4\nmerge 2,3 bc\n\n", &mut p);
        let names: Vec<&str> = p.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "bc"]);
        assert_eq!(p[1].repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(p[1].sessions(), 5);
        let mut p = proposals();
        let out = drive("merge 9,1\nrename x\nfrob\nmerge 4,1\n", &mut p);
        assert!(out.contains("usage: merge") && out.contains("usage: rename") && out.contains("unknown command"));
        assert_eq!(
            (p.len(), p[0].name.as_str(), p[0].repos.len()),
            (3, "a", 2),
            "merge keeps the first position and name"
        );
    }

    #[test]
    fn projects_are_created_once_and_repos_remembered() {
        let (_d, conn) = crate::db::temp_db();
        let p = proposals();
        assert_eq!(create_projects(&conn, &p).unwrap().len(), 4);
        assert!(create_projects(&conn, &p).unwrap().is_empty());
        assert_eq!(tickets::repo_path(&conn, "a").unwrap().as_deref(), Some("/r/a"));
    }

    #[test]
    fn flow_with_yes_creates_projects_and_skips_palaver() {
        let d = Dir::new();
        let cp = fixture(&d);
        let ctx = Ctx::for_test(d.path(), crate::tmux::Tmux::private("onboard"));
        let mut out = Vec::new();
        let opts = Options { yes: true, start_session: false, days: 60 };
        run(&mut Io { input: &mut Cursor::new(String::new()), out: &mut out }, &ctx, &cp, &opts).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("2 sessions")
                && out.contains("Created: alpha, beta")
                && out.contains("Palaver: not available"),
            "{out}"
        );
        let conn = ctx.db().unwrap();
        assert_eq!(projects::list(&conn).unwrap().len(), 2);
        // re-running leaves existing projects alone
        let mut out = Vec::new();
        run(&mut Io { input: &mut Cursor::new(String::new()), out: &mut out }, &ctx, &cp, &opts).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("Nothing new to create"));
    }

    #[test]
    fn interactive_flow_links_palaver_by_name_and_number() {
        let d = Dir::new();
        let cp = fixture(&d);
        let mut ctx = Ctx::for_test(d.path(), crate::tmux::Tmux::private("onboard2"));
        ctx.palaver_bin = palaver::tests::stub(&d);
        let mut out = Vec::new();
        // rename alpha -> Alpha, accept, create, link Alpha by default (same name), beta to #2, no session
        let input = "rename 1 Alpha\n\n\n\n2\nn\n";
        let opts = Options { yes: false, start_session: false, days: 60 };
        run(&mut Io { input: &mut Cursor::new(input.to_string()), out: &mut out }, &ctx, &cp, &opts).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Alpha -> Palaver Alpha") && out.contains("beta -> Palaver Beta"), "{out}");
        let conn = ctx.db().unwrap();
        assert_eq!(projects::palaver_of(&conn, "beta").unwrap().as_deref(), Some("Beta"));
    }

    #[test]
    fn session_prompt_names_projects_transcripts_and_filing_commands() {
        let mut p = proposals();
        p[0].repos[0].transcript_dirs.insert(PathBuf::from("/home/.claude/projects/-r-a"));
        let s = session_prompt(&p[..1], "/bin/cockpit");
        for needle in [
            "- a\n",
            "/r/a",
            "/home/.claude/projects/-r-a",
            "/bin/cockpit tool ticket.add",
            "project.set_services",
            "Do not file anything yet",
        ] {
            assert!(s.contains(needle), "{needle}");
        }
    }
}
