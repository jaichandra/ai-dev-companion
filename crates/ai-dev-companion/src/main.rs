//! The framework's own placeholder binary (a distribution crate has its own
//! `main` with its profile and packs). It exists so the guards can be run and
//! tested as real child processes, the way Claude's hooks invoke them.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = ai_dev_companion::guard::cli::run_if_guard(&args) {
        std::process::exit(code);
    }
    eprintln!("ai-dev-companion: no such command (the framework binary only runs `guard`)");
    std::process::exit(64);
}
