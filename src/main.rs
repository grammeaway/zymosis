mod cli;
mod config;
mod model;
mod store;

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    match args.command {
        Some(cmd) => {
            if let Err(e) = cli::run(cmd) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        None => {
            // Interactive TUI is a later phase.
            eprintln!("zym: interactive TUI not implemented yet — run `zym --help` for CLI commands");
            std::process::exit(2);
        }
    }
}
