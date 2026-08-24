//! Domain model: tasks, subtasks, and derived lifecycle status.
//!
//! Lifecycle status is never stored — it is computed from `last_updated` and
//! the configured thresholds, so it can never drift out of sync with the data.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Unix-epoch seconds. Stored directly (not `SystemTime`) so the on-disk and
/// export formats are stable and human-inspectable.
pub type Timestamp = u64;

/// Current wall-clock time as unix-epoch seconds.
pub fn now() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

/// Age-ordered lifecycle bands. Derived from a task's age, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Hot,
    Decaying,
    Dormant,
    Bubbling,
}

/// Time thresholds partitioning a task's age into lifecycle bands.
/// Phase 3 `Config` owns and constructs these; for lifecycle math they are the
/// only external input besides the task's own age.
///
/// For sane (monotonic) behaviour, `hot_window <= dormant_after`. Config
/// validation (Phase 3) enforces the ordering; the derivation itself stays a
/// total function even if misconfigured.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Age below which a task is Hot.
    pub hot_window: Duration,
    /// Age at which a Decaying task becomes Dormant.
    pub dormant_after: Duration,
    /// Extra age *beyond* `dormant_after` at which a Dormant task starts Bubbling.
    pub bubble_after: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubTask {
    pub title: String,
    pub done: bool,
}

/// A freeform note on a task: additional detail/consideration, timestamped so
/// the history is visible when a bubbling task resurfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub text: String,
    pub created: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub title: String,
    /// Timestamped details/considerations, appended over time.
    #[serde(default)]
    pub notes: Vec<Note>,
    pub created: Timestamp,
    pub last_updated: Timestamp,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub subtasks: Vec<SubTask>,
    /// Freeform categories, normalized lowercase (e.g. "monitoring", "perf").
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Normalize a tag: trimmed + lowercased, or `None` if empty.
pub fn normalize_tag(s: &str) -> Option<String> {
    let t = s.trim().to_lowercase();
    (!t.is_empty()).then_some(t)
}

impl Task {
    pub fn new(id: u64, title: impl Into<String>) -> Self {
        let ts = now();
        Self {
            id,
            title: title.into(),
            notes: Vec::new(),
            created: ts,
            last_updated: ts,
            done: false,
            subtasks: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// Add a tag (normalized, deduped). Returns whether it was newly added.
    pub fn add_tag(&mut self, tag: &str) -> bool {
        match normalize_tag(tag) {
            Some(t) if !self.tags.contains(&t) => {
                self.tags.push(t);
                true
            }
            _ => false,
        }
    }

    /// Append a note (trimmed + timestamped). Returns whether one was added
    /// (empty/whitespace text is ignored). Does not touch; the caller does, so
    /// note-taking follows the same "interaction resets to Hot" rule as tags.
    pub fn add_note(&mut self, text: &str) -> bool {
        let t = text.trim();
        if t.is_empty() {
            return false;
        }
        self.notes.push(Note {
            text: t.to_string(),
            created: now(),
        });
        true
    }

    /// Remove a tag. Returns whether one was removed.
    pub fn remove_tag(&mut self, tag: &str) -> bool {
        let Some(t) = normalize_tag(tag) else {
            return false;
        };
        let before = self.tags.len();
        self.tags.retain(|x| x != &t);
        self.tags.len() != before
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        normalize_tag(tag).is_some_and(|t| self.tags.contains(&t))
    }

    /// Mark freshly relevant (revive / "still relevant"): resets age to Hot.
    pub fn touch(&mut self) {
        self.last_updated = now();
    }

    /// Age at reference time `now_ts`. Saturates so a clock that briefly runs
    /// backwards yields age 0 rather than underflowing.
    pub fn age(&self, now_ts: Timestamp) -> Duration {
        Duration::from_secs(now_ts.saturating_sub(self.last_updated))
    }

    /// Derived lifecycle status. Bands are half-open and contiguous, so every
    /// possible age maps to exactly one status.
    pub fn status(&self, t: &Thresholds, now_ts: Timestamp) -> Status {
        let age = self.age(now_ts);
        if age < t.hot_window {
            Status::Hot
        } else if age < t.dormant_after {
            Status::Decaying
        } else if age < t.dormant_after + t.bubble_after {
            Status::Dormant
        } else {
            Status::Bubbling
        }
    }

    /// Next free id for a task list (max + 1, or 1 when empty).
    pub fn next_id(tasks: &[Task]) -> u64 {
        tasks.iter().map(|t| t.id).max().map_or(1, |m| m + 1)
    }

    /// (completed_subtasks, total_subtasks).
    pub fn progress(&self) -> (usize, usize) {
        let done = self.subtasks.iter().filter(|s| s.done).count();
        (done, self.subtasks.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn rank(s: Status) -> u8 {
        match s {
            Status::Hot => 0,
            Status::Decaying => 1,
            Status::Dormant => 2,
            Status::Bubbling => 3,
        }
    }

    prop_compose! {
        /// Ordered, valid thresholds: hot_window <= dormant_after, bubble_after >= 0.
        fn thresholds()(hw in 1u64..=1000, extra in 0u64..=1000, bub in 0u64..=1000)
            -> Thresholds {
            Thresholds {
                hot_window: Duration::from_secs(hw),
                dormant_after: Duration::from_secs(hw + extra),
                bubble_after: Duration::from_secs(bub),
            }
        }
    }

    #[test]
    fn tags_normalize_and_dedup() {
        let mut t = Task::new(1, "x");
        assert!(t.add_tag("  Monitoring "));
        assert!(!t.add_tag("monitoring")); // dup after normalize
        assert!(!t.add_tag("   ")); // empty ignored
        assert_eq!(t.tags, vec!["monitoring"]);
        assert!(t.has_tag("MONITORING"));
        assert!(t.remove_tag("monitoring"));
        assert!(t.tags.is_empty());
    }

    #[test]
    fn notes_append_and_ignore_empty() {
        let mut t = Task::new(1, "x");
        assert!(t.add_note("  consider caching  "));
        assert!(!t.add_note("   ")); // whitespace-only ignored
        assert_eq!(t.notes.len(), 1);
        assert_eq!(t.notes[0].text, "consider caching"); // trimmed
    }

    proptest! {
        /// A freshly touched task is always Hot (age 0 < hot_window).
        #[test]
        fn touched_task_is_hot(t in thresholds()) {
            let mut task = Task::new(1, "x");
            task.last_updated = 0; // pretend it went stale
            task.touch();
            prop_assert_eq!(task.status(&t, task.last_updated), Status::Hot);
        }

        /// More age never yields a hotter (lower-ranked) band.
        #[test]
        fn status_monotonic_in_age(
            t in thresholds(),
            last in 0u64..1_000_000,
            a in 0u64..10_000,
            b in 0u64..10_000,
        ) {
            let mut task = Task::new(1, "x");
            task.last_updated = last;
            let (younger, older) = if a <= b { (a, b) } else { (b, a) };
            let s1 = task.status(&t, last + younger);
            let s2 = task.status(&t, last + older);
            prop_assert!(rank(s2) >= rank(s1));
        }

        /// progress() never reports more done than total.
        #[test]
        fn progress_within_bounds(flags in prop::collection::vec(any::<bool>(), 0..20)) {
            let mut task = Task::new(1, "x");
            task.subtasks = flags
                .iter()
                .map(|&done| SubTask { title: "s".into(), done })
                .collect();
            let (done, total) = task.progress();
            prop_assert_eq!(total, flags.len());
            prop_assert!(done <= total);
        }
    }
}
