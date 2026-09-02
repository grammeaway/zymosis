//! Application configuration: human-editable TOML at the platform config path,
//! merged over built-in defaults.
//!
//! Adding an option later = add a field + a default in `Default`. Container-level
//! `#[serde(default)]` means a missing file *or* a missing field falls back to
//! the default, so old config files keep working. Durations are written as human
//! spans ("2d", "36h") rather than opaque seconds.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::{Thresholds, normalize_board_name};

/// A duration written as `<n><unit>`, unit one of s/m/h/d/w. Single unit only.
// ponytail: single-unit spans only ("2d", not "1d12h"); add multi-unit parsing
// if a real config ever needs the precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span(pub Duration);

impl Span {
    /// Parse a human span ("2d", "36h"); the inverse of `as_human`.
    pub fn parse(s: &str) -> Result<Span, String> {
        parse_span(s).map(Span)
    }

    /// Render as the largest exact human unit ("14d" -> "2w").
    pub fn as_human(&self) -> String {
        format_span(self.0)
    }
}

fn parse_span(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("missing unit in '{s}' (use s/m/h/d/w)"))?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("bad number in '{s}'"))?;
    let size = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(format!("unknown unit '{unit}' in '{s}' (use s/m/h/d/w)")),
    };
    Ok(Duration::from_secs(n.saturating_mul(size)))
}

fn format_span(d: Duration) -> String {
    let secs = d.as_secs();
    for (unit, size) in [("w", 604_800u64), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs >= size && secs % size == 0 {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

impl Serialize for Span {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_span(self.0))
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_span(&s).map(Span).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Age below which a task is Hot.
    pub hot_window: Span,
    /// Age at which a Decaying task becomes Dormant.
    pub dormant_after: Span,
    /// Extra age beyond `dormant_after` at which a Dormant task starts Bubbling.
    pub bubble_after: Span,
    /// Where the task store lives.
    pub storage_path: PathBuf,
    /// TUI redraw cap (frames/sec).
    pub tick_fps: u32,
    /// Named color theme (resolved by the TUI; unknown names fall back to the
    /// default). Kept a free string so old files load and new themes need no
    /// schema change.
    pub theme: String,
    /// The board the CLI/TUI act on when no `-b` flag is given.
    pub active_board: String,
    /// Per-board overrides, serialized as `[boards.<name>]` tables. Kept last so
    /// the scalar fields serialize before it (TOML requires tables come after).
    pub boards: BTreeMap<String, BoardOverrides>,
}

/// Optional per-board overrides; every `None` field inherits the global config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoardOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hot_window: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dormant_after: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bubble_after: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
}

impl BoardOverrides {
    pub fn is_empty(&self) -> bool {
        self.hot_window.is_none()
            && self.dormant_after.is_none()
            && self.bubble_after.is_none()
            && self.theme.is_none()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hot_window: Span(Duration::from_secs(2 * 86_400)),
            dormant_after: Span(Duration::from_secs(14 * 86_400)),
            bubble_after: Span(Duration::from_secs(30 * 86_400)),
            storage_path: default_storage_path(),
            tick_fps: 12,
            theme: "neon_purple".into(),
            active_board: "default".into(),
            boards: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn thresholds(&self) -> Thresholds {
        Thresholds {
            hot_window: self.hot_window.0,
            dormant_after: self.dormant_after.0,
            bubble_after: self.bubble_after.0,
        }
    }

    /// A copy of this config with `board`'s overrides applied. Unknown board
    /// (or no overrides) yields an unchanged clone.
    pub fn effective(&self, board: &str) -> Config {
        let mut cfg = self.clone();
        if let Some(o) = self.boards.get(board) {
            if let Some(v) = o.hot_window {
                cfg.hot_window = v;
            }
            if let Some(v) = o.dormant_after {
                cfg.dormant_after = v;
            }
            if let Some(v) = o.bubble_after {
                cfg.bubble_after = v;
            }
            if let Some(v) = &o.theme {
                cfg.theme = v.clone();
            }
        }
        cfg
    }

    /// Enforce the ordering the lifecycle math assumes for monotonic behaviour,
    /// globally and for every board's effective thresholds.
    pub fn validate(&self) -> Result<(), String> {
        if self.hot_window.0 > self.dormant_after.0 {
            return Err(format!(
                "hot_window ({}) must be <= dormant_after ({})",
                format_span(self.hot_window.0),
                format_span(self.dormant_after.0),
            ));
        }
        if self.tick_fps == 0 {
            return Err("tick_fps must be >= 1".into());
        }
        for name in self.boards.keys() {
            normalize_board_name(name)?;
            let e = self.effective(name);
            if e.hot_window.0 > e.dormant_after.0 {
                return Err(format!(
                    "board '{name}': hot_window ({}) must be <= dormant_after ({})",
                    format_span(e.hot_window.0),
                    format_span(e.dormant_after.0),
                ));
            }
        }
        Ok(())
    }
}

/// `~/.config/zym/config.toml` (platform-appropriate).
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zym")
        .join("config.toml")
}

/// `~/.local/share/zym/tasks.json` (platform-appropriate).
fn default_storage_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zym")
        .join("tasks.json")
}

pub fn load() -> Result<Config, String> {
    load_from(&config_path())
}

/// Load config, merging file over defaults. Missing file → all defaults.
pub fn load_from(path: &Path) -> Result<Config, String> {
    let cfg = match fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| format!("parse {}: {e}", path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    cfg.validate()?;
    Ok(cfg)
}

pub fn save(cfg: &Config) -> io::Result<()> {
    save_to(cfg, &config_path())
}

pub fn save_to(cfg: &Config, path: &Path) -> io::Result<()> {
    let text = toml::to_string(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_parse_format_roundtrip() {
        assert_eq!(parse_span("14d").unwrap(), Duration::from_secs(14 * 86_400));
        assert_eq!(parse_span(" 36h ").unwrap(), Duration::from_secs(36 * 3600));
        assert_eq!(format_span(Duration::from_secs(14 * 86_400)), "2w"); // 14d == 2 weeks, largest exact unit wins
        assert_eq!(format_span(Duration::from_secs(30 * 86_400)), "30d"); // not a whole week
        assert_eq!(format_span(Duration::from_secs(36 * 3600)), "36h");
        assert_eq!(format_span(Duration::from_secs(90)), "90s");
        assert!(parse_span("abc").is_err());
        assert!(parse_span("5").is_err());
        assert!(parse_span("10x").is_err());
    }

    #[test]
    fn missing_file_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_from(&dir.path().join("none.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_file_merges_over_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "hot_window = \"1d\"\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.hot_window, Span(Duration::from_secs(86_400)));
        assert_eq!(cfg.dormant_after, Config::default().dormant_after); // untouched
        assert_eq!(cfg.tick_fps, Config::default().tick_fps);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::default();
        save_to(&cfg, &path).unwrap();
        assert_eq!(load_from(&path).unwrap(), cfg);
    }

    #[test]
    fn validation_rejects_bad_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "hot_window = \"20d\"\ndormant_after = \"14d\"\n").unwrap();
        assert!(load_from(&path).is_err());
    }

    #[test]
    fn old_file_defaults_active_board_and_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "hot_window = \"1d\"\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.active_board, "default");
        assert!(cfg.boards.is_empty());
    }

    #[test]
    fn board_override_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[boards.work]\nhot_window = \"1d\"\n").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(
            cfg.boards["work"].hot_window,
            Some(Span(Duration::from_secs(86_400)))
        );
        save_to(&cfg, &path).unwrap();
        assert_eq!(load_from(&path).unwrap(), cfg);
    }

    #[test]
    fn effective_overrides_win_and_others_inherit() {
        let mut cfg = Config::default();
        cfg.boards.insert(
            "work".into(),
            BoardOverrides {
                hot_window: Some(Span(Duration::from_secs(3600))),
                ..Default::default()
            },
        );
        let e = cfg.effective("work");
        assert_eq!(e.hot_window, Span(Duration::from_secs(3600)));
        assert_eq!(e.dormant_after, cfg.dormant_after); // inherited
        assert_eq!(cfg.effective("nonexistent"), cfg); // no overrides
    }

    #[test]
    fn validation_rejects_board_that_breaks_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // global dormant_after is 14d; a 20d hot_window override breaks ordering.
        fs::write(&path, "[boards.work]\nhot_window = \"20d\"\n").unwrap();
        assert!(load_from(&path).is_err());
    }

    proptest::proptest! {
        #[test]
        fn effective_field_is_override_or_global(
            hw in proptest::option::of(1u64..1000),
            da in proptest::option::of(1u64..1000),
            ba in proptest::option::of(0u64..1000),
            th in proptest::option::of("[a-z]{1,6}"),
        ) {
            let mut cfg = Config::default();
            let o = BoardOverrides {
                hot_window: hw.map(|s| Span(Duration::from_secs(s))),
                dormant_after: da.map(|s| Span(Duration::from_secs(s))),
                bubble_after: ba.map(|s| Span(Duration::from_secs(s))),
                theme: th.clone(),
            };
            cfg.boards.insert("b".into(), o.clone());
            let e = cfg.effective("b");
            proptest::prop_assert_eq!(e.hot_window, o.hot_window.unwrap_or(cfg.hot_window));
            proptest::prop_assert_eq!(e.dormant_after, o.dormant_after.unwrap_or(cfg.dormant_after));
            proptest::prop_assert_eq!(e.bubble_after, o.bubble_after.unwrap_or(cfg.bubble_after));
            proptest::prop_assert_eq!(e.theme, o.theme.unwrap_or(cfg.theme));
        }
    }
}
