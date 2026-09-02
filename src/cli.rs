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
    /// Act on this board for this invocation (overrides the active board).
    #[arg(short = 'b', long, global = true)]
    pub board: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Add a task.
    Add {
        title: String,
        /// Repeatable: --note "a" --note "b".
        #[arg(long = "note")]
        notes: Vec<String>,
        /// Repeatable: --subtask "a" --subtask "b".
        #[arg(long = "subtask")]
        subtasks: Vec<String>,
        /// Repeatable: --tag monitoring --tag perf.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// List tasks (hides done + dormant by default).
    List {
        #[arg(long, value_enum)]
        status: Option<StatusFilter>,
        /// Only tasks carrying this tag.
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mark a task done.
    Done { id: u64 },
    /// Edit a task's title (bumps it back to Hot). Notes are managed with `note`.
    Edit {
        id: u64,
        #[arg(long)]
        title: Option<String>,
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
    /// Show a task with its (indexed) subtasks.
    Show { id: u64 },
    /// Manage boards (independent task lists).
    Board {
        #[command(subcommand)]
        action: BoardCmd,
    },
    /// Print the version (also available as --version / -V).
    Version,
    /// Work with a task's subtasks (index is 1-based).
    Subtask {
        #[command(subcommand)]
        action: SubtaskCmd,
    },
    /// Work with a task's notes (details/considerations; index is 1-based).
    Note {
        #[command(subcommand)]
        action: NoteCmd,
    },
    /// Work with a task's tags/categories.
    Tag {
        #[command(subcommand)]
        action: TagCmd,
    },
}

#[derive(Subcommand)]
pub enum BoardCmd {
    /// List boards (active one marked with *).
    List,
    /// Create an empty board.
    Add { name: String },
    /// Switch the active board (persisted to config).
    Use { name: String },
    /// Rename a board (moves its file + any config overrides).
    Rename { old: String, new: String },
    /// Delete a board's file (not the active or last board).
    #[command(visible_alias = "delete")]
    Rm { name: String },
}

#[derive(Subcommand)]
pub enum TagCmd {
    /// Add a tag to a task.
    Add { task_id: u64, tag: String },
    /// Remove a tag from a task.
    Rm { task_id: u64, tag: String },
}

#[derive(Subcommand)]
pub enum NoteCmd {
    /// Append a note.
    Add { task_id: u64, text: String },
    /// Remove a note.
    Rm { task_id: u64, index: usize },
}

#[derive(Subcommand)]
pub enum SubtaskCmd {
    /// Append a subtask.
    Add { task_id: u64, title: String },
    /// Toggle a subtask done.
    Done { task_id: u64, index: usize },
    /// Remove a subtask.
    Rm { task_id: u64, index: usize },
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

/// Trailing "  #a #b" for display, or empty when untagged.
fn tag_suffix(t: &Task) -> String {
    if t.tags.is_empty() {
        String::new()
    } else {
        format!(
            "  {}",
            t.tags
                .iter()
                .map(|x| format!("#{x}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

fn status_str(s: Status) -> &'static str {
    match s {
        Status::Hot => "hot",
        Status::Decaying => "decaying",
        Status::Dormant => "dormant",
        Status::Bubbling => "bubbling",
    }
}

/// Filtered + sorted (most-recently-updated first) view for `list`.
fn select<'a>(
    tasks: &'a [Task],
    cfg: &Config,
    filter: Option<StatusFilter>,
    tag: Option<&str>,
    all: bool,
    now: Timestamp,
) -> Vec<&'a Task> {
    let mut view: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            if let Some(tg) = tag {
                if !t.has_tag(tg) {
                    return false;
                }
            }
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

/// Resolve a 1-based index into a 0-based slot within `len`, or a clear error.
fn one_based(index: usize, len: usize, what: &str, task_id: u64) -> Result<usize, String> {
    index
        .checked_sub(1)
        .filter(|&i| i < len)
        .ok_or_else(|| format!("no {what} {index} on task {task_id}"))
}

/// Compact relative age ("3d ago") using the largest whole unit.
fn ago(now: Timestamp, then: Timestamp) -> String {
    let secs = now.saturating_sub(then);
    for (unit, size) in [("w", 604_800u64), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs >= size {
            return format!("{}{unit} ago", secs / size);
        }
    }
    "just now".into()
}

fn io_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// The board a task command acts on: `-b` flag beats the config's active board.
/// Both go through `normalize_board_name` so a typo fails rather than targeting
/// a phantom board.
fn resolve_board(flag: Option<&str>, active: &str) -> Result<String, String> {
    model::normalize_board_name(flag.unwrap_or(active))
}

fn run_board(action: BoardCmd, cfg: &mut Config) -> Result<(), String> {
    match action {
        BoardCmd::List => {
            let mut names = store::list_boards(&cfg.storage_path).map_err(io_err)?;
            if !names.contains(&cfg.active_board) {
                names.push(cfg.active_board.clone());
                names.sort();
            }
            for name in names {
                let mark = if name == cfg.active_board { "*" } else { " " };
                println!("{mark} {name}");
            }
        }
        BoardCmd::Add { name } => {
            let name = model::normalize_board_name(&name)?;
            let path = store::board_path(&cfg.storage_path, &name);
            if path.exists() {
                return Err(format!("board '{name}' already exists"));
            }
            store::save(&path, &[]).map_err(io_err)?;
            println!("created board '{name}'");
        }
        BoardCmd::Use { name } => {
            let name = model::normalize_board_name(&name)?;
            if !store::board_path(&cfg.storage_path, &name).exists() {
                return Err(format!(
                    "no board '{name}' — create it with `zym board add`"
                ));
            }
            cfg.active_board = name.clone();
            config::save(cfg).map_err(io_err)?;
            println!("switched to board '{name}'");
        }
        BoardCmd::Rename { old, new } => {
            let old = model::normalize_board_name(&old)?;
            let new = model::normalize_board_name(&new)?;
            let from = store::board_path(&cfg.storage_path, &old);
            let to = store::board_path(&cfg.storage_path, &new);
            if !from.exists() {
                return Err(format!("no board '{old}'"));
            }
            if old != new && to.exists() {
                return Err(format!("board '{new}' already exists"));
            }
            std::fs::rename(&from, &to).map_err(io_err)?;
            if let Some(o) = cfg.boards.remove(&old) {
                cfg.boards.insert(new.clone(), o);
            }
            if cfg.active_board == old {
                cfg.active_board = new.clone();
            }
            config::save(cfg).map_err(io_err)?;
            println!("renamed board '{old}' to '{new}'");
        }
        BoardCmd::Rm { name } => {
            let name = model::normalize_board_name(&name)?;
            if name == cfg.active_board {
                return Err(format!("cannot remove the active board '{name}'"));
            }
            let boards = store::list_boards(&cfg.storage_path).map_err(io_err)?;
            if boards.len() <= 1 {
                return Err("cannot remove the only board".into());
            }
            let path = store::board_path(&cfg.storage_path, &name);
            if !path.exists() {
                return Err(format!("no board '{name}'"));
            }
            std::fs::remove_file(&path).map_err(io_err)?;
            if cfg.boards.remove(&name).is_some() {
                config::save(cfg).map_err(io_err)?;
            }
            println!("removed board '{name}'");
        }
    }
    Ok(())
}

pub fn run(cli: Cli) -> Result<(), String> {
    let cmd = cli.command.expect("run called without a subcommand");
    if let Command::Version = cmd {
        println!("zym {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut cfg = config::load()?;

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
            println!("active_board = {}", cfg.active_board);
            println!("---");
            print!("{}", toml::to_string(&cfg).map_err(io_err)?);
        }
        return Ok(());
    }

    // Board management needs cfg but not the task list.
    if let Command::Board { action } = cmd {
        return run_board(action, &mut cfg);
    }

    let board = resolve_board(cli.board.as_deref(), &cfg.active_board)?;
    let ecfg = cfg.effective(&board);
    store::migrate_legacy(&cfg.storage_path).map_err(io_err)?;
    let store_path = store::board_path(&cfg.storage_path, &board);
    let mut tasks = store::load(&store_path).map_err(io_err)?;
    let now = model::now();

    match cmd {
        Command::Add {
            title,
            notes,
            subtasks,
            tags,
        } => {
            let mut task = Task::new(Task::next_id(&tasks), title);
            for n in &notes {
                task.add_note(n);
            }
            task.subtasks = subtasks
                .into_iter()
                .map(|title| model::SubTask { title, done: false })
                .collect();
            for tag in &tags {
                task.add_tag(tag);
            }
            let id = task.id;
            tasks.push(task);
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("added task {id}");
        }
        Command::List {
            status,
            tag,
            all,
            json,
        } => {
            let view = select(&tasks, &ecfg, status, tag.as_deref(), all, now);
            if json {
                let out: Vec<TaskView> = view
                    .iter()
                    .map(|t| TaskView {
                        task: t,
                        status: status_str(status_of(t, &ecfg, now)),
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out).map_err(io_err)?);
            } else if view.is_empty() {
                println!("(no tasks)");
            } else {
                for t in view {
                    let (done, total) = t.progress();
                    let prog = if total > 0 {
                        format!(" [{done}/{total}]")
                    } else {
                        String::new()
                    };
                    let mark = if t.done { "x" } else { " " };
                    let tags = tag_suffix(t);
                    println!(
                        "{:>4} [{}] {:8} {}{}{}",
                        t.id,
                        mark,
                        status_str(status_of(t, &ecfg, now)),
                        t.title,
                        prog,
                        tags
                    );
                }
            }
        }
        Command::Done { id } => {
            find_mut(&mut tasks, id)?.done = true;
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("marked task {id} done");
        }
        Command::Edit { id, title } => {
            let t = find_mut(&mut tasks, id)?;
            if let Some(title) = title {
                t.title = title;
            }
            t.touch();
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("edited task {id}");
        }
        Command::Revive { id } => {
            find_mut(&mut tasks, id)?.touch();
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("revived task {id}");
        }
        Command::Rm { id } => {
            let before = tasks.len();
            tasks.retain(|t| t.id != id);
            if tasks.len() == before {
                return Err(format!("no task with id {id}"));
            }
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("removed task {id}");
        }
        Command::Export { path } => {
            store::export(&path, &tasks).map_err(io_err)?;
            println!("exported {} task(s) to {}", tasks.len(), path.display());
        }
        Command::Import { path } => {
            let mut incoming = store::import(&path).map_err(io_err)?;
            let mut id = Task::next_id(&tasks);
            for t in &mut incoming {
                t.id = id;
                id += 1;
            }
            let n = incoming.len();
            tasks.extend(incoming);
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("imported {n} task(s)");
        }
        Command::Show { id } => {
            let t = tasks
                .iter()
                .find(|t| t.id == id)
                .ok_or_else(|| format!("no task with id {id}"))?;
            let mark = if t.done { "x" } else { " " };
            println!("#{} [{}] {}", t.id, mark, t.title);
            println!("status: {}", status_str(status_of(t, &ecfg, now)));
            if !t.tags.is_empty() {
                println!("tags:  {}", tag_suffix(t).trim());
            }
            if !t.notes.is_empty() {
                println!("notes:");
                for (i, n) in t.notes.iter().enumerate() {
                    println!("  {:>2}. {}  ({})", i + 1, n.text, ago(now, n.created));
                }
            }
            if t.subtasks.is_empty() {
                println!("(no subtasks)");
            } else {
                for (i, s) in t.subtasks.iter().enumerate() {
                    let m = if s.done { "x" } else { " " };
                    println!("  {:>2}. [{}] {}", i + 1, m, s.title);
                }
            }
        }
        Command::Subtask { action } => {
            let msg = match action {
                SubtaskCmd::Add { task_id, title } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    t.subtasks.push(model::SubTask { title, done: false });
                    t.touch();
                    format!("added subtask to task {task_id}")
                }
                SubtaskCmd::Done { task_id, index } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    let i = one_based(index, t.subtasks.len(), "subtask", task_id)?;
                    t.subtasks[i].done = !t.subtasks[i].done;
                    t.touch();
                    format!("toggled subtask {index} on task {task_id}")
                }
                SubtaskCmd::Rm { task_id, index } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    let i = one_based(index, t.subtasks.len(), "subtask", task_id)?;
                    t.subtasks.remove(i);
                    t.touch();
                    format!("removed subtask {index} from task {task_id}")
                }
            };
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("{msg}");
        }
        Command::Tag { action } => {
            let msg = match action {
                TagCmd::Add { task_id, tag } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    if !t.add_tag(&tag) {
                        return Err(format!("task {task_id} already has tag or tag is empty"));
                    }
                    t.touch();
                    format!("tagged task {task_id}")
                }
                TagCmd::Rm { task_id, tag } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    if !t.remove_tag(&tag) {
                        return Err(format!("task {task_id} has no tag '{tag}'"));
                    }
                    t.touch();
                    format!("untagged task {task_id}")
                }
            };
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("{msg}");
        }
        Command::Note { action } => {
            let msg = match action {
                NoteCmd::Add { task_id, text } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    if !t.add_note(&text) {
                        return Err(format!("note text for task {task_id} is empty"));
                    }
                    t.touch();
                    format!("noted task {task_id}")
                }
                NoteCmd::Rm { task_id, index } => {
                    let t = find_mut(&mut tasks, task_id)?;
                    let i = one_based(index, t.notes.len(), "note", task_id)?;
                    t.notes.remove(i);
                    t.touch();
                    format!("removed note {index} from task {task_id}")
                }
            };
            store::save(&store_path, &tasks).map_err(io_err)?;
            println!("{msg}");
        }
        Command::Config { .. } | Command::Board { .. } | Command::Version => {
            unreachable!("handled above")
        }
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
    fn resolve_board_flag_beats_config_and_trims() {
        assert_eq!(resolve_board(Some("  Work "), "default").unwrap(), "Work");
        assert_eq!(resolve_board(None, "work").unwrap(), "work");
        assert!(resolve_board(Some("a/b"), "default").is_err());
    }

    #[test]
    fn one_based_is_one_based_and_bounded() {
        assert_eq!(one_based(1, 2, "subtask", 1).unwrap(), 0);
        assert_eq!(one_based(2, 2, "subtask", 1).unwrap(), 1);
        assert!(one_based(0, 2, "subtask", 1).is_err()); // 0 is not valid (1-based)
        assert!(one_based(3, 2, "subtask", 1).is_err()); // out of range
    }

    #[test]
    fn ago_picks_largest_whole_unit() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(3 * 86_400 + 5, 0), "3d ago");
        assert_eq!(ago(90 * 60, 0), "1h ago");
        assert_eq!(ago(50, 100), "just now"); // clock jumped back: saturates
    }

    #[test]
    fn next_id_increments() {
        assert_eq!(Task::next_id(&[]), 1);
        assert_eq!(Task::next_id(&[aged(1, 0, false), aged(4, 0, false)]), 5);
    }

    #[test]
    fn select_hides_done_and_dormant_by_default() {
        let cfg = Config::default(); // hot<2d, dormant>14d
        let now = NOW;
        let day = 86_400;
        let tasks = vec![
            aged(1, 0, false),        // hot
            aged(2, 0, true),         // hot but done
            aged(3, 20 * day, false), // dormant
        ];
        let ids: Vec<u64> = select(&tasks, &cfg, None, None, false, now)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![1]);

        // --all shows everything
        assert_eq!(select(&tasks, &cfg, None, None, true, now).len(), 3);

        // status filter targets a band regardless of done/dormant defaults
        let dormant: Vec<u64> = select(&tasks, &cfg, Some(StatusFilter::Dormant), None, false, now)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(dormant, vec![3]);
    }

    #[test]
    fn select_filters_by_tag() {
        let cfg = Config::default();
        let now = NOW;
        let mut a = aged(1, 0, false);
        a.add_tag("perf");
        let mut b = aged(2, 0, false);
        b.add_tag("org");
        let tasks = vec![a, b];
        let ids: Vec<u64> = select(&tasks, &cfg, None, Some("perf"), false, now)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn select_sorts_recent_first() {
        let cfg = Config::default();
        let now = NOW;
        let tasks = vec![aged(1, 500, false), aged(2, 10, false), aged(3, 100, false)];
        let ids: Vec<u64> = select(&tasks, &cfg, None, None, true, now)
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![2, 3, 1]);
    }
}
