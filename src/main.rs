mod cli;
mod config;
mod model;
mod store;
mod tui;

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
            if let Err(e) = tui::run() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}
