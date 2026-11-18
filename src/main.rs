use cockpit::{cli, daemon};

fn main() {
    cockpit::util::build_id(); // before anything can replace the binary on disk
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cockpit: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(mut args: Vec<String>) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("add" | "ready" | "show" | "claim" | "comment" | "close") => cli::ticket_command(args),
        Some("tool") => cli::tool_command(args),
        Some("daemon") => {
            args.remove(0);
            daemon::command(args)
        }
        Some("hook") => {
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            cockpit::sessions::run_hook(&input); // never fails: a hook must not break a session
            Ok(())
        }
        Some("install" | "uninstall") => cockpit::install::command(args),
        Some("onboard") => cockpit::onboard::command(args),
        Some(other) => anyhow::bail!("unknown command {other}"),
        None => cockpit::tui::launch(),
    }
}
