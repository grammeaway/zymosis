//! Persistence: crash-safe JSON storage plus export/import.
//!
//! One format (pretty JSON) for both the live store and exports, so export is
//! the same writer pointed at an arbitrary path. Writes are atomic (temp file
//! in the same directory + `rename`), the one corner we never cut: a crash
//! mid-write can never corrupt the task list.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::model::Task;

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn read_json(path: &Path) -> io::Result<Vec<Task>> {
    let data = fs::read_to_string(path)?;
    if data.trim().is_empty() {
        return Ok(Vec::new()); // freshly-created / blank file
    }
    serde_json::from_str(&data).map_err(to_io)
}

fn write_atomic(path: &Path, tasks: &[Task]) -> io::Result<()> {
    let json = serde_json::to_string_pretty(tasks).map_err(to_io)?;

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }

    // Temp file in the SAME directory so the rename stays on one filesystem
    // (rename is only atomic within a filesystem). Pid-tagged to avoid two
    // concurrent invocations clobbering each other's temp.
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("tasks.json");
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));

    let write = || -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?; // durable on disk before we swap it in
        fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = fs::remove_file(&tmp); // don't leave junk behind on failure
    })
}

/// Load the store, treating a missing file as an empty list (first run).
pub fn load(path: &Path) -> io::Result<Vec<Task>> {
    match read_json(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        other => other,
    }
}

/// Persist the store atomically.
pub fn save(path: &Path, tasks: &[Task]) -> io::Result<()> {
    write_atomic(path, tasks)
}

/// Write tasks to an arbitrary path (same format as the store).
pub fn export(path: &Path, tasks: &[Task]) -> io::Result<()> {
    write_atomic(path, tasks)
}

/// Read tasks from an arbitrary path. Unlike `load`, a missing file is an error
/// (the user named it explicitly).
pub fn import(path: &Path) -> io::Result<Vec<Task>> {
    read_json(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SubTask;
    use proptest::prelude::*;

    prop_compose! {
        fn a_subtask()(title in ".*", done in any::<bool>()) -> SubTask {
            SubTask { title, done }
        }
    }

    prop_compose! {
        fn a_task()(
            id in any::<u64>(),
            title in ".*",
            notes in ".*",
            created in any::<u64>(),
            last_updated in any::<u64>(),
            done in any::<bool>(),
            subtasks in prop::collection::vec(a_subtask(), 0..5),
        ) -> Task {
            Task { id, title, notes, created, last_updated, done, subtasks }
        }
    }

    proptest! {
        /// save -> load is the identity, for any task list.
        #[test]
        fn save_load_round_trip(tasks in prop::collection::vec(a_task(), 0..10)) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tasks.json");
            save(&path, &tasks).unwrap();
            prop_assert_eq!(load(&path).unwrap(), tasks);
        }
    }

    #[test]
    fn load_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn import_missing_is_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(import(&dir.path().join("nope.json")).is_err());
    }

    #[test]
    fn empty_file_is_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        fs::write(&path, "   \n").unwrap();
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn save_creates_parent_dirs_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deep/tasks.json");
        save(&path, &[Task::new(1, "x")]).unwrap();
        assert!(path.exists());
        // no leftover .tmp siblings
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
    }
}
