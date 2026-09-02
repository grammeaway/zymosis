mod cli;
mod config;
mod model;
mod store;
mod tui;

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    match args.command {
        Some(_) => {
            if let Err(e) = cli::run(args) {
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
