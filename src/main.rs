// Temporary: model/store/config are exercised by tests and later phases before
// main() wires them in. Removed in Phase 4 once the CLI uses everything.
#![allow(dead_code)]

mod cli;
mod config;
mod model;
mod store;

fn main() {
    // Phase 4 wires CLI dispatch here; TUI is a later phase.
    println!("zym: scaffold in place");
}
