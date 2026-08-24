//! CLI surface over the data + config layer. Doubles as the scripting/agentic
//! interface and as the way to exercise everything before the TUI exists.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::config::{self, Config};
use crate::model::{self, Status, Task, Timestamp};
use crate::store;

#[derive(Parser)]
#[command(name = "zym", about = "A fermenting todo list", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Add a task.
    Add {
        title: String,
        #[arg(long)]
        note: Option<String>,
        /// Repeatable: --subtask "a" --subtask "b".
        #[arg(long = "subtask")]
        subtasks: Vec<String>,
    },
    /// List tasks (hides done + dormant by default).
    List {
        #[arg(long, value_enum)]
        status: Option<StatusFilter>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mark a task done.
    Done { id: u64 },
    /// Edit a task's title/note (bumps it back to Hot).
    Edit {
        id: u64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Mark a task still-relevant, reviving it to Hot.
    Revive { id: u64 },
    /// Delete a task.
    Rm { id: u64 },
    /// Export all tasks to a JSON file.
    Export { path: PathBuf },
    /// Append tasks from a JSON file (ids are reassigned).
    Import { path: PathBuf },
    /// Show config (and paths), or --init to write the default file.
    Config {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        init: bool,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum StatusFilter {
    Hot,
    Decaying,
    Dormant,
    Bubbling,
}

fn status_of(t: &Task, cfg: &Config, now: Timestamp) -> Status {
    t.status(&cfg.thresholds(), now)
}

fn status_str(s: Status) -> &'static str {
    match s {
        Status::Hot => "hot",
        Status::Decaying => "decaying",
        Status::Dormant => "dormant",
        Status::Bubbling => "bubbling",
    }
}

fn next_id(tasks: &[Task]) -> u64 {
    tasks.iter().map(|t| t.id).max().map_or(1, |m| m + 1)
}

/// Filtered + sorted (most-recently-updated first) view for `list`.
fn select<'a>(
    tasks: &'a [Task],
    cfg: &Config,
    filter: Option<StatusFilter>,
    all: bool,
    now: Timestamp,
) -> Vec<&'a Task> {
    let mut view: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            let st = status_of(t, cfg, now);
            match filter {
                Some(f) => matches!(
                    (f, st),
                    (StatusFilter::Hot, Status::Hot)
                        | (StatusFilter::Decaying, Status::Decaying)
                        | (StatusFilter::Dormant, Status::Dormant)
                        | (StatusFilter::Bubbling, Status::Bubbling)
                ),
                None if all => true,
                None => !t.done && st != Status::Dormant,
            }
        })
        .collect();
    view.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    view
}

fn find_mut<'a>(tasks: &'a mut [Task], id: u64) -> Result<&'a mut Task, String> {
    tasks
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("no task with id {id}"))
}

fn io_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub fn run(cmd: Command) -> Result<(), String> {
    let cfg = config::load()?;

    // Config command doesn't touch the store.
    if let Command::Config { json, init } = cmd {
        if init {
            config::save(&cfg).map_err(io_err)?;
            println!("wrote {}", config::config_path().display());
        } else if json {
            println!("{}", serde_json::to_string_pretty(&cfg).map_err(io_err)?);
        } else {
            println!("config_path  = {}", config::config_path().display());
            println!("storage_path = {}", cfg.storage_path.display());
            println!("---");
            print!("{}", toml::to_string(&cfg).map_err(io_err)?);
        }
        return Ok(());
    }

    let mut tasks = store::load(&cfg.storage_path).map_err(io_err)?;
    let now = model::now();

    match cmd {
        Command::Add { title, note, subtasks } => {
            let mut task = Task::new(next_id(&tasks), title);
            if let Some(n) = note {
                task.notes = n;
            }
            task.subtasks = subtasks
                .into_iter()
                .map(|title| model::SubTask { title, done: false })
                .collect();
            let id = task.id;
            tasks.push(task);
            store::save(&cfg.storage_path, &tasks).map_err(io_err)?;
            println!("added task {id}");
        }
        Command::List { status, all, json } => {
            let view = select(&tasks, &cfg, status, all, now);
            if json {
                let out: Vec<TaskView> = view
                    .iter()
                    .map(|t| TaskView { task: t, status: status_str(status_of(t, &cfg, now)) })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out).map_err(io_err)?);
            } else if view.is_empty() {
                println!("(no tasks)");
            } else {
                for t in view {
                    let (done, total) = t.progress();
                    let prog = if total > 0 { format!(" [{done}/{total}]") } else { String::new() };
                    let mark = if t.done { "x" } else { " " };
                    println!(
                        "{:>4} [{}] {:8} {}{}",
                        t.id,
                        mark,
                        status_str(status_of(t, &cfg, now)),
                        t.title,
                        prog
                    );
                }
            }
        }
        Command::Done { id } => {
            find_mut(&mut tasks, id)?.done = true;
            store::save(&cfg.storage_path, &tasks).map_err(io_err)?;
            println!("marked task {id} done");
        }
        Command::Edit { id, title, note } => {
            let t = find_mut(&mut tasks, id)?;
            if let Some(title) = title {
                t.title = title;
            }
            if let Some(note) = note {
                t.notes = note;
            }
            t.touch();
            store::save(&cfg.storage_path, &tasks).map_err(io_err)?;
            println!("edited task {id}");
        }
        Command::Revive { id } => {
            find_mut(&mut tasks, id)?.touch();
            store::save(&cfg.storage_path, &tasks).map_err(io_err)?;
            println!("revived task {id}");
        }
        Command::Rm { id } => {
            let before = tasks.len();
            tasks.retain(|t| t.id != id);
            if tasks.len() == before {
                return Err(format!("no task with id {id}"));
            }
            store::save(&cfg.storage_path, &tasks).map_err(io_err)?;
            println!("removed task {id}");
        }
        Command::Export { path } => {
            store::export(&path, &tasks).map_err(io_err)?;
            println!("exported {} task(s) to {}", tasks.len(), path.display());
        }
        Command::Import { path } => {
            let mut incoming = store::import(&path).map_err(io_err)?;
            let mut id = next_id(&tasks);
            for t in &mut incoming {
                t.id = id;
                id += 1;
            }
            let n = incoming.len();
            tasks.extend(incoming);
            store::save(&cfg.storage_path, &tasks).map_err(io_err)?;
            println!("imported {n} task(s)");
        }
        Command::Config { .. } => unreachable!("handled above"),
    }
    Ok(())
}

#[derive(Serialize)]
struct TaskView<'a> {
    #[serde(flatten)]
    task: &'a Task,
    status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = 100_000_000;

    fn aged(id: u64, secs_ago: u64, done: bool) -> Task {
        let mut t = Task::new(id, format!("t{id}"));
        t.done = done;
        t.last_updated = NOW - secs_ago;
        t
    }

    #[test]
    fn next_id_increments() {
        assert_eq!(next_id(&[]), 1);
        assert_eq!(next_id(&[aged(1, 0, false), aged(4, 0, false)]), 5);
    }

    #[test]
    fn select_hides_done_and_dormant_by_default() {
        let cfg = Config::default(); // hot<2d, dormant>14d
        let now = NOW;
        let day = 86_400;
        let tasks = vec![
            aged(1, 0, false),         // hot
            aged(2, 0, true),          // hot but done
            aged(3, 20 * day, false),  // dormant
        ];
        let ids: Vec<u64> = select(&tasks, &cfg, None, false, now).iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1]);

        // --all shows everything
        assert_eq!(select(&tasks, &cfg, None, true, now).len(), 3);

        // status filter targets a band regardless of done/dormant defaults
        let dormant: Vec<u64> = select(&tasks, &cfg, Some(StatusFilter::Dormant), false, now)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(dormant, vec![3]);
    }

    #[test]
    fn select_sorts_recent_first() {
        let cfg = Config::default();
        let now = NOW;
        let tasks = vec![aged(1, 500, false), aged(2, 10, false), aged(3, 100, false)];
        let ids: Vec<u64> = select(&tasks, &cfg, None, true, now).iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![2, 3, 1]);
    }
}
