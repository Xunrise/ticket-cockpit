use cockpit::{cli, daemon};

fn main() {
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
        // TODO(plan step 12): seed a fresh install from existing Claude transcripts
        Some("onboard") => anyhow::bail!("`onboard` is not implemented yet (TODO: plan step 12)"),
        Some(other) => anyhow::bail!("unknown command {other}"),
        None => cockpit::tui::launch(),
    }
}
