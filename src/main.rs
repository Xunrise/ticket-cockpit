use cockpit::cli;

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

fn run(args: Vec<String>) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("add" | "ready" | "show" | "claim" | "comment" | "close") => cli::ticket_command(args),
        Some("hook") => {
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            cockpit::sessions::run_hook(&input); // never fails: a hook must not break a session
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command {other}"),
        None => {
            println!("TUI not built yet");
            Ok(())
        }
    }
}
