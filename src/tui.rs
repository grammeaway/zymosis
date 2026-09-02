//! Interactive TUI.
//! Slice 1: read-only list, status styling, navigation.
//! Slice 2: add/edit/done/revive/delete with atomic save-on-change.
//! Slice 3: capped tick loop + juice (bubbling animation, hot breathe, decay
//!          color ramp) and a dormant-section toggle.
//! Slice 4: expandable subtasks (inline). Enter/→ expands a task; space toggles
//!          the highlighted subtask. Adding/removing subtasks stays in the CLI.
//!
//! Terminal setup/teardown goes through `ratatui::try_init`/`try_restore`, which
//! install a panic hook that restores the terminal.

use std::collections::HashSet;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{self, BoardOverrides, Config, Span as CfgSpan};
use crate::model::{self, Status, SubTask, Task, Thresholds};
use crate::store;

pub fn run() -> Result<(), String> {
    let cfg = config::load()?;
    store::migrate_legacy(&cfg.storage_path).map_err(|e| e.to_string())?;
    let path = store::board_path(&cfg.storage_path, &cfg.active_board);
    let tasks = store::load(&path).map_err(|e| e.to_string())?;
    let mut app = App::new(cfg, tasks);
    let mut term = ratatui::try_init().map_err(|e| format!("no terminal available: {e}"))?;
    let res = app.run(&mut term);
    let _ = ratatui::try_restore();
    res
}

enum Mode {
    Normal,
    Help,
    Add,
    Edit(u64),
    EditSub(u64, usize),
    EditNote(u64, usize),
    AddSub(u64),
    AddNote(u64),
    EditTags(u64),
    Search,
    Config,
    EditCfg(CfgField),
    Boards { names: Vec<String>, sel: usize },
    AddBoard,
    RenameBoard { old: String },
    ConfirmDeleteBoard { name: String },
}

/// Which config layer an edit targets: the global config, or the current
/// board's overrides.
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Global,
    Board,
}

/// The three task sections. Dormant and Done are fully separate views, not a
/// filter over the active list — Tab cycles Active → Dormant → Done.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Active,
    Dormant,
    Done,
}

impl View {
    fn next(self) -> View {
        match self {
            View::Active => View::Dormant,
            View::Dormant => View::Done,
            View::Done => View::Active,
        }
    }
}

/// The subset of config editable in the TUI (storage_path stays CLI-only, since
/// changing it live would mean reloading the store).
#[derive(Clone, Copy, PartialEq)]
enum CfgField {
    HotWindow,
    DormantAfter,
    BubbleAfter,
    TickFps,
    Theme,
}

const CFG_FIELDS: [(CfgField, &str); 5] = [
    (CfgField::HotWindow, "hot_window"),
    (CfgField::DormantAfter, "dormant_after"),
    (CfgField::BubbleAfter, "bubble_after"),
    (CfgField::TickFps, "tick_fps"),
    (CfgField::Theme, "theme"),
];

fn cfg_field_index(f: CfgField) -> usize {
    CFG_FIELDS.iter().position(|(x, _)| *x == f).unwrap()
}

fn cfg_value(cfg: &Config, f: CfgField) -> String {
    match f {
        CfgField::HotWindow => cfg.hot_window.as_human(),
        CfgField::DormantAfter => cfg.dormant_after.as_human(),
        CfgField::BubbleAfter => cfg.bubble_after.as_human(),
        CfgField::TickFps => cfg.tick_fps.to_string(),
        CfgField::Theme => cfg.theme.clone(),
    }
}

fn parse_theme(input: &str) -> Result<String, String> {
    let name = input.trim();
    if THEME_NAMES.contains(&name) {
        Ok(name.to_string())
    } else {
        Err(format!(
            "unknown theme '{name}' (try: {})",
            THEME_NAMES.join(", ")
        ))
    }
}

/// Apply one field edit to a copy of the config, parsing + validating. Global
/// scope edits the shared config; board scope edits `board`'s overrides, where
/// empty input clears the override (and drops the entry once it's empty).
/// tick_fps is global-only. Pure so the parse/validate branch is testable.
fn apply_cfg(
    cfg: &Config,
    board: &str,
    scope: Scope,
    f: CfgField,
    input: &str,
) -> Result<Config, String> {
    let mut next = cfg.clone();
    match scope {
        Scope::Global => match f {
            CfgField::HotWindow => next.hot_window = CfgSpan::parse(input)?,
            CfgField::DormantAfter => next.dormant_after = CfgSpan::parse(input)?,
            CfgField::BubbleAfter => next.bubble_after = CfgSpan::parse(input)?,
            CfgField::TickFps => {
                next.tick_fps = input
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad number '{}'", input.trim()))?
            }
            CfgField::Theme => next.theme = parse_theme(input)?,
        },
        Scope::Board => {
            if matches!(f, CfgField::TickFps) {
                return Err("tick_fps is global-only".into());
            }
            let empty = input.trim().is_empty();
            let entry = next.boards.entry(board.to_string()).or_default();
            match f {
                CfgField::HotWindow => {
                    entry.hot_window = if empty {
                        None
                    } else {
                        Some(CfgSpan::parse(input)?)
                    }
                }
                CfgField::DormantAfter => {
                    entry.dormant_after = if empty {
                        None
                    } else {
                        Some(CfgSpan::parse(input)?)
                    }
                }
                CfgField::BubbleAfter => {
                    entry.bubble_after = if empty {
                        None
                    } else {
                        Some(CfgSpan::parse(input)?)
                    }
                }
                CfgField::Theme => {
                    entry.theme = if empty {
                        None
                    } else {
                        Some(parse_theme(input)?)
                    }
                }
                CfgField::TickFps => unreachable!("rejected above"),
            }
            if next.boards[board].is_empty() {
                next.boards.remove(board);
            }
        }
    }
    next.validate()?;
    Ok(next)
}

fn override_is_set(o: &BoardOverrides, f: CfgField) -> bool {
    match f {
        CfgField::HotWindow => o.hot_window.is_some(),
        CfgField::DormantAfter => o.dormant_after.is_some(),
        CfgField::BubbleAfter => o.bubble_after.is_some(),
        CfgField::Theme => o.theme.is_some(),
        CfgField::TickFps => false,
    }
}

/// Raw string of an override field (for edit prefill); empty when unset.
fn override_raw(o: &BoardOverrides, f: CfgField) -> String {
    match f {
        CfgField::HotWindow => o.hot_window.map(|s| s.as_human()).unwrap_or_default(),
        CfgField::DormantAfter => o.dormant_after.map(|s| s.as_human()).unwrap_or_default(),
        CfgField::BubbleAfter => o.bubble_after.map(|s| s.as_human()).unwrap_or_default(),
        CfgField::Theme => o.theme.clone().unwrap_or_default(),
        CfgField::TickFps => String::new(),
    }
}

/// A rendered line: a task, one of its subtasks, or one of its notes (indices
/// into `tasks`).
#[derive(Clone, Copy)]
enum Row {
    Task(usize),
    Sub(usize, usize),
    Note(usize, usize),
}

struct App {
    cfg: Config,
    board: String,
    theme: Theme,
    tasks: Vec<Task>,
    selected: usize,
    mode: Mode,
    input: String,
    cursor: usize, // char index into `input`
    status: Option<String>,
    view: View,
    expanded: HashSet<u64>,
    cfg_sel: usize,
    cfg_scope: Scope,
    g_pending: bool, // saw a lone `g`, waiting for the second in `gg`
    frame: u64,
    quit: bool,
}

impl App {
    fn new(cfg: Config, tasks: Vec<Task>) -> Self {
        let board = cfg.active_board.clone();
        let theme = Theme::named(&cfg.effective(&board).theme);
        Self {
            cfg,
            board,
            theme,
            tasks,
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            cursor: 0,
            status: None,
            view: View::Active,
            expanded: HashSet::new(),
            cfg_sel: 0,
            cfg_scope: Scope::Global,
            g_pending: false,
            frame: 0,
            quit: false,
        }
    }

    /// Config with the active board's overrides applied — the source of truth
    /// for thresholds and theme.
    fn ecfg(&self) -> Config {
        self.cfg.effective(&self.board)
    }

    /// Task indices belonging to the current view, ordered by lifecycle
    /// priority (Hot/Bubbling rise) and recency within a band. Done tasks live
    /// only in the Done view; Active and Dormant split the rest by status.
    fn visible(&self) -> Vec<usize> {
        let now = model::now();
        let th = self.ecfg().thresholds();
        let mut idx: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| {
                let t = &self.tasks[i];
                match self.view {
                    View::Done => t.done,
                    View::Dormant => !t.done && t.status(&th, now) == Status::Dormant,
                    View::Active => !t.done && t.status(&th, now) != Status::Dormant,
                }
            })
            .collect();
        idx.sort_by(|&a, &b| {
            let ra = sort_rank(self.tasks[a].status(&th, now));
            let rb = sort_rank(self.tasks[b].status(&th, now));
            ra.cmp(&rb)
                .then(self.tasks[b].last_updated.cmp(&self.tasks[a].last_updated))
        });
        idx
    }

    /// Flattened rows: each visible task, followed by its subtasks if expanded.
    fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for ti in self.visible() {
            out.push(Row::Task(ti));
            if self.expanded.contains(&self.tasks[ti].id) {
                for si in 0..self.tasks[ti].subtasks.len() {
                    out.push(Row::Sub(ti, si));
                }
                for ni in 0..self.tasks[ti].notes.len() {
                    out.push(Row::Note(ti, ni));
                }
            }
        }
        out
    }

    fn selected_row(&self) -> Option<Row> {
        self.rows().get(self.selected).copied()
    }

    /// Task index of the selected row, only when a *task* row is highlighted.
    fn selected_task(&self) -> Option<usize> {
        match self.selected_row() {
            Some(Row::Task(ti)) => Some(ti),
            _ => None,
        }
    }

    /// Task index the selected row belongs to (task row, or a subtask's parent).
    fn row_task(&self) -> Option<usize> {
        match self.selected_row() {
            Some(Row::Task(ti)) | Some(Row::Sub(ti, _)) | Some(Row::Note(ti, _)) => Some(ti),
            None => None,
        }
    }

    fn has_animation(&self) -> bool {
        let now = model::now();
        let th = self.ecfg().thresholds();
        self.visible().iter().any(|&i| {
            matches!(
                self.tasks[i].status(&th, now),
                Status::Hot | Status::Bubbling
            )
        })
    }

    fn run(&mut self, term: &mut DefaultTerminal) -> Result<(), String> {
        let mut dirty = true;
        while !self.quit {
            // Recomputed each loop so a tick_fps change in the config screen is live.
            let tick = Duration::from_millis((1000 / self.cfg.tick_fps.max(1)) as u64);
            if dirty {
                term.draw(|f| self.draw(f)).map_err(|e| e.to_string())?;
                dirty = false;
            }
            if event::poll(tick).map_err(|e| e.to_string())? {
                if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
                    if k.kind == KeyEventKind::Press {
                        match self.mode {
                            Mode::Normal => self.on_normal_key(k.code),
                            Mode::Help => self.mode = Mode::Normal, // any key closes help
                            Mode::Config => self.on_config_key(k.code),
                            Mode::Boards { .. } => self.on_boards_key(k.code),
                            Mode::ConfirmDeleteBoard { .. } => {
                                self.on_confirm_delete_board_key(k.code)
                            }
                            _ => self.on_input_key(k.code, k.modifiers),
                        }
                        dirty = true;
                    }
                }
            } else {
                self.frame = self.frame.wrapping_add(1);
                if self.has_animation() {
                    dirty = true;
                }
            }
        }
        Ok(())
    }

    fn on_normal_key(&mut self, code: KeyCode) {
        self.status = None;
        let rows = self.rows().len();
        // Any key but a second `g` breaks a pending `gg`.
        let g_was_pending = std::mem::take(&mut self.g_pending);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('g') => {
                if g_was_pending {
                    self.selected = 0; // gg → top
                } else {
                    self.g_pending = true;
                }
            }
            KeyCode::Char('G') => self.selected = rows.saturating_sub(1), // bottom
            KeyCode::Char('/') => {
                self.clear_input();
                self.mode = Mode::Search;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = move_selection(self.selected, rows, 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = move_selection(self.selected, rows, -1)
            }
            KeyCode::Tab => {
                self.view = self.view.next();
                self.selected = 0;
            }
            KeyCode::Enter | KeyCode::Right => self.toggle_expand(),
            KeyCode::Char('a') => {
                self.clear_input();
                self.mode = Mode::Add;
            }
            KeyCode::Char('s') => {
                if let Some(ti) = self.row_task() {
                    self.clear_input();
                    self.mode = Mode::AddSub(self.tasks[ti].id);
                }
            }
            KeyCode::Char('n') => {
                if let Some(ti) = self.row_task() {
                    self.clear_input();
                    self.mode = Mode::AddNote(self.tasks[ti].id);
                }
            }
            KeyCode::Char('e') => match self.selected_row() {
                Some(Row::Task(ti)) => {
                    self.set_input(self.tasks[ti].title.clone());
                    self.mode = Mode::Edit(self.tasks[ti].id);
                }
                Some(Row::Sub(ti, si)) => {
                    self.set_input(self.tasks[ti].subtasks[si].title.clone());
                    self.mode = Mode::EditSub(self.tasks[ti].id, si);
                }
                Some(Row::Note(ti, ni)) => {
                    self.set_input(self.tasks[ti].notes[ni].text.clone());
                    self.mode = Mode::EditNote(self.tasks[ti].id, ni);
                }
                None => {}
            },
            KeyCode::Char('t') => {
                if let Some(i) = self.selected_task() {
                    self.set_input(self.tasks[i].tags.join(" "));
                    self.mode = Mode::EditTags(self.tasks[i].id);
                }
            }
            KeyCode::Char('d') | KeyCode::Char(' ') => {
                if self.toggle_selected() {
                    self.persist();
                }
            }
            KeyCode::Char('r') => {
                if self.revive_selected() {
                    self.persist();
                }
            }
            KeyCode::Char('c') => {
                self.cfg_sel = 0;
                self.cfg_scope = Scope::Global;
                self.mode = Mode::Config;
            }
            KeyCode::Char('b') => self.open_boards(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('y') => self.yank_selected(),
            KeyCode::Char('x') | KeyCode::Delete => {
                if self.remove_selected() {
                    self.persist();
                }
            }
            _ => {}
        }
    }

    fn on_config_key(&mut self, code: KeyCode) {
        self.status = None;
        match code {
            KeyCode::Char('q') | KeyCode::Char('c') | KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Tab => {
                self.cfg_scope = match self.cfg_scope {
                    Scope::Global => Scope::Board,
                    Scope::Board => Scope::Global,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cfg_sel = move_selection(self.cfg_sel, CFG_FIELDS.len(), 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.cfg_sel = move_selection(self.cfg_sel, CFG_FIELDS.len(), -1)
            }
            KeyCode::Enter => {
                let f = CFG_FIELDS[self.cfg_sel].0;
                if self.cfg_scope == Scope::Board && f == CfgField::TickFps {
                    self.status = Some("tick_fps is global-only".into());
                } else {
                    self.set_input(self.cfg_edit_value(f));
                    self.mode = Mode::EditCfg(f);
                }
            }
            _ => {}
        }
    }

    /// Value to prefill the edit line: the global value, or the current override
    /// (empty when unset, so board-scope editing starts blank and empty clears).
    fn cfg_edit_value(&self, f: CfgField) -> String {
        match self.cfg_scope {
            Scope::Global => cfg_value(&self.cfg, f),
            Scope::Board => self
                .cfg
                .boards
                .get(&self.board)
                .map(|o| override_raw(o, f))
                .unwrap_or_default(),
        }
    }

    /// Board names for the picker, always including the active board even if its
    /// file doesn't exist on disk yet.
    fn board_names(&self) -> Vec<String> {
        let mut names = store::list_boards(&self.cfg.storage_path).unwrap_or_default();
        if !names.contains(&self.board) {
            names.push(self.board.clone());
            names.sort();
        }
        names
    }

    fn open_boards(&mut self) {
        let names = self.board_names();
        let sel = names.iter().position(|n| *n == self.board).unwrap_or(0);
        self.mode = Mode::Boards { names, sel };
    }

    fn on_boards_key(&mut self, code: KeyCode) {
        self.status = None;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Down | KeyCode::Char('j') => {
                if let Mode::Boards { names, sel } = &mut self.mode {
                    *sel = move_selection(*sel, names.len(), 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Mode::Boards { names, sel } = &mut self.mode {
                    *sel = move_selection(*sel, names.len(), -1);
                }
            }
            KeyCode::Enter => {
                if let Mode::Boards { names, sel } = &self.mode {
                    self.switch_board(names[*sel].clone());
                }
            }
            KeyCode::Char('a') => {
                self.clear_input();
                self.mode = Mode::AddBoard;
            }
            KeyCode::Char('r') => {
                let old = match &self.mode {
                    Mode::Boards { names, sel } => names.get(*sel).cloned(),
                    _ => None,
                };
                if let Some(old) = old {
                    self.set_input(old.clone());
                    self.mode = Mode::RenameBoard { old };
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let name = match &self.mode {
                    Mode::Boards { names, sel } => names.get(*sel).cloned(),
                    _ => None,
                };
                if let Some(name) = name {
                    match board_deletable(&name, &self.board, self.board_names().len()) {
                        Ok(()) => self.mode = Mode::ConfirmDeleteBoard { name },
                        Err(e) => self.status = Some(e.into()),
                    }
                }
            }
            _ => {}
        }
    }

    fn on_confirm_delete_board_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Mode::ConfirmDeleteBoard { name } = &self.mode {
                    self.delete_board(name.clone());
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                self.open_boards()
            }
            _ => {}
        }
    }

    /// Delete a board's file and drop any config override. Guarded upstream
    /// (`board_deletable`), so it never removes the active or only board.
    fn delete_board(&mut self, name: String) {
        let path = store::board_path(&self.cfg.storage_path, &name);
        if let Err(e) = std::fs::remove_file(&path) {
            self.status = Some(format!("delete failed: {e}"));
        } else {
            if self.cfg.boards.remove(&name).is_some() {
                self.save_cfg();
            }
            self.status = Some(format!("deleted {name}"));
        }
        self.open_boards();
    }

    /// Rename a board: move its file, carry over any config override and the
    /// active-board pointer, then reopen the picker.
    fn rename_board(&mut self, old: String, new: String) {
        let new = match model::normalize_board_name(&new) {
            Ok(n) => n,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };
        if new == old {
            self.open_boards();
            return;
        }
        let to = store::board_path(&self.cfg.storage_path, &new);
        if to.exists() {
            self.status = Some(format!("board '{new}' already exists"));
            return;
        }
        let from = store::board_path(&self.cfg.storage_path, &old);
        if let Err(e) = std::fs::rename(&from, &to) {
            self.status = Some(format!("rename failed: {e}"));
            return;
        }
        if let Some(o) = self.cfg.boards.remove(&old) {
            self.cfg.boards.insert(new.clone(), o);
        }
        if self.board == old {
            self.board = new.clone();
            self.cfg.active_board = new.clone();
        }
        self.save_cfg();
        self.status = Some(format!("renamed to {new}"));
        self.open_boards();
    }

    /// Load a board's tasks and make it active (persisted). Per-mutation saves
    /// mean there's nothing to flush for the board we're leaving.
    fn switch_board(&mut self, name: String) {
        let path = store::board_path(&self.cfg.storage_path, &name);
        match store::load(&path) {
            Ok(tasks) => {
                self.tasks = tasks;
                self.board = name.clone();
                self.cfg.active_board = name.clone();
                self.theme = Theme::named(&self.ecfg().theme);
                self.selected = 0;
                self.expanded.clear();
                self.view = View::Active;
                self.mode = Mode::Normal;
                self.save_cfg();
                self.status = Some(format!("switched to {name}"));
            }
            Err(e) => self.status = Some(format!("load failed: {e}")),
        }
    }

    fn create_board(&mut self, name: String) {
        let name = match model::normalize_board_name(&name) {
            Ok(n) => n,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };
        let path = store::board_path(&self.cfg.storage_path, &name);
        if path.exists() {
            self.status = Some(format!("board '{name}' already exists"));
            return;
        }
        if let Err(e) = store::save(&path, &[]) {
            self.status = Some(format!("save failed: {e}"));
            return;
        }
        self.switch_board(name);
    }

    fn on_input_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let len = self.input.chars().count();
        match code {
            KeyCode::Esc => {
                match self.mode {
                    Mode::EditCfg(_) => self.mode = Mode::Config,
                    Mode::AddBoard | Mode::RenameBoard { .. } => self.open_boards(),
                    _ => self.mode = Mode::Normal,
                }
                self.clear_input();
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                // Kill from cursor back to the start of the line (readline Ctrl+U).
                let b = byte_at(&self.input, self.cursor);
                self.input.replace_range(..b, "");
                self.cursor = 0;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let b = byte_at(&self.input, self.cursor - 1);
                self.input.remove(b);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let b = byte_at(&self.input, self.cursor);
                self.input.remove(b);
            }
            KeyCode::Char(c) if !ctrl => {
                let b = byte_at(&self.input, self.cursor);
                self.input.insert(b, c);
                self.cursor += 1;
            }
            // Search confirms in place: selection already tracked the first
            // match incrementally as you typed.
            KeyCode::Enter if matches!(self.mode, Mode::Search) => {
                self.mode = Mode::Normal;
                self.clear_input();
            }
            // Config edits parse/validate and stay on the config screen; a bad
            // value shows an error and keeps the input for a retry.
            KeyCode::Enter if matches!(self.mode, Mode::EditCfg(_)) => {
                let Mode::EditCfg(f) = self.mode else {
                    unreachable!()
                };
                match apply_cfg(&self.cfg, &self.board, self.cfg_scope, f, &self.input) {
                    Ok(next) => {
                        self.cfg = next;
                        self.theme = Theme::named(&self.ecfg().theme); // live theme switch
                        self.save_cfg();
                        self.mode = Mode::Config;
                        self.clear_input();
                    }
                    Err(e) => self.status = Some(e),
                }
            }
            KeyCode::Enter => {
                let title = self.input.trim().to_string();
                // EditTags applies even when empty (empty input clears all tags);
                // the other input modes treat empty as a no-op.
                if let Mode::EditTags(id) = self.mode {
                    self.apply_tags(id, title);
                    self.persist();
                } else if let Mode::AddBoard = self.mode {
                    self.create_board(title); // sets its own mode; persists per-board
                    self.clear_input();
                    return;
                } else if let Mode::RenameBoard { old } = &self.mode {
                    let old = old.clone();
                    self.rename_board(old, title);
                    self.clear_input();
                    return;
                } else if !title.is_empty() {
                    match self.mode {
                        Mode::Add => self.add_task(title),
                        Mode::Edit(id) => self.apply_edit(id, title),
                        Mode::EditSub(id, si) => self.apply_edit_sub(id, si, title),
                        Mode::EditNote(id, ni) => self.apply_edit_note(id, ni, title),
                        Mode::AddSub(id) => self.add_subtask(id, title),
                        Mode::AddNote(id) => self.add_note(id, title),
                        Mode::Normal
                        | Mode::Help
                        | Mode::Config
                        | Mode::EditCfg(_)
                        | Mode::EditTags(_)
                        | Mode::Search
                        | Mode::Boards { .. }
                        | Mode::AddBoard
                        | Mode::RenameBoard { .. }
                        | Mode::ConfirmDeleteBoard { .. } => {}
                    }
                    self.persist();
                }
                self.mode = Mode::Normal;
                self.clear_input();
            }
            _ => {}
        }
        // Incremental search: keep the highlight on the first match as the
        // query changes (Esc/Enter already left Search mode, so this is skipped).
        if matches!(self.mode, Mode::Search) {
            if let Some(i) = self.find_match(&self.input) {
                self.selected = i;
            }
        }
    }

    /// Row index of the first task matching `query` (case-insensitive substring
    /// of the title or any tag). None when the query is empty or nothing hits.
    fn find_match(&self, query: &str) -> Option<usize> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return None;
        }
        self.rows().iter().position(|r| {
            matches!(r, Row::Task(ti) if
                self.tasks[*ti].title.to_lowercase().contains(&q)
                    || self.tasks[*ti].tags.iter().any(|t| t.contains(&q)))
        })
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    fn set_input(&mut self, s: String) {
        self.cursor = s.chars().count();
        self.input = s;
    }

    /// Move the top-level highlight onto the task with `id` after an
    /// interaction re-sorts the list, so selection follows the task it acted
    /// on. Falls back to clamping when the task left the current view.
    fn select_task(&mut self, id: u64) {
        let rows = self.rows();
        match rows
            .iter()
            .position(|r| matches!(r, Row::Task(ti) if self.tasks[*ti].id == id))
        {
            Some(i) => self.selected = i,
            None => self.selected = self.selected.min(rows.len().saturating_sub(1)),
        }
    }

    // --- pure mutations (no I/O; caller persists) ---

    fn add_task(&mut self, title: String) {
        self.tasks
            .push(Task::new(Task::next_id(&self.tasks), title));
        self.selected = 0;
    }

    fn apply_edit(&mut self, id: u64, title: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.title = title;
            t.touch();
        }
        self.select_task(id);
    }

    fn apply_edit_sub(&mut self, id: u64, si: usize, title: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            if let Some(s) = t.subtasks.get_mut(si) {
                s.title = title;
                t.touch();
            }
        }
        self.select_task(id);
    }

    fn apply_edit_note(&mut self, id: u64, ni: usize, text: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            if let Some(n) = t.notes.get_mut(ni) {
                n.text = text;
                t.touch();
            }
        }
        self.select_task(id);
    }

    /// Re-set a task's whole tag set from a space-separated input, reusing
    /// `add_tag` for normalization + dedup. A leading `#` (as shown in chips)
    /// is tolerated. Editing tags touches the task, like every interaction.
    fn apply_tags(&mut self, id: u64, input: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.tags.clear();
            for tok in input.split_whitespace() {
                t.add_tag(tok.trim_start_matches('#'));
            }
            t.touch();
        }
        self.select_task(id);
    }

    fn add_subtask(&mut self, id: u64, title: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.subtasks.push(SubTask { title, done: false });
            t.touch();
        }
        self.expanded.insert(id); // reveal what was just added
        self.select_task(id); // follow the task it jumped to the top
    }

    fn add_note(&mut self, id: u64, text: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.add_note(&text);
            t.touch();
        }
        self.expanded.insert(id); // reveal what was just added
        self.select_task(id); // follow the task it jumped to the top
    }

    /// Toggle done on the highlighted row: a task, or a subtask (which also
    /// touches its parent, since interacting keeps a task relevant).
    fn toggle_selected(&mut self) -> bool {
        match self.selected_row() {
            Some(Row::Task(ti)) => {
                self.tasks[ti].done = !self.tasks[ti].done;
                true
            }
            Some(Row::Sub(ti, si)) => {
                self.tasks[ti].subtasks[si].done = !self.tasks[ti].subtasks[si].done;
                self.tasks[ti].touch();
                true
            }
            Some(Row::Note(_, _)) | None => false, // notes have no done state
        }
    }

    fn toggle_expand(&mut self) {
        if let Some(ti) = self.selected_task() {
            let id = self.tasks[ti].id;
            if !self.expanded.remove(&id) {
                self.expanded.insert(id);
            }
        }
    }

    /// Bring a task back to Hot/active: clears `done` (so it leaves the Done
    /// view) and touches it. This is the path back from Dormant or Done.
    fn revive_selected(&mut self) -> bool {
        match self.selected_task() {
            Some(ti) => {
                self.tasks[ti].done = false;
                self.tasks[ti].touch();
                let id = self.tasks[ti].id;
                self.select_task(id);
                true
            }
            None => false,
        }
    }

    /// Delete the highlighted row: a task (with its subtasks), or a single
    /// subtask (touching its parent).
    fn remove_selected(&mut self) -> bool {
        match self.selected_row() {
            Some(Row::Task(ti)) => {
                self.expanded.remove(&self.tasks[ti].id);
                self.tasks.remove(ti);
            }
            Some(Row::Sub(ti, si)) => {
                self.tasks[ti].subtasks.remove(si);
                self.tasks[ti].touch();
            }
            Some(Row::Note(ti, ni)) => {
                self.tasks[ti].notes.remove(ni);
                self.tasks[ti].touch();
            }
            None => return false,
        }
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
        true
    }

    /// Copy the selected row's text (task title, subtask title, or note) to the
    /// system clipboard via OSC 52 — terminal-native, no deps, works over SSH.
    fn yank_selected(&mut self) {
        let text = match self.selected_row() {
            Some(Row::Task(ti)) => Some(self.tasks[ti].title.clone()),
            Some(Row::Sub(ti, si)) => Some(self.tasks[ti].subtasks[si].title.clone()),
            Some(Row::Note(ti, ni)) => Some(self.tasks[ti].notes[ni].text.clone()),
            None => None,
        };
        if let Some(t) = text {
            copy_to_clipboard(&t);
            self.status = Some(format!("yanked: {}", tail(&t, 40)));
        }
    }

    fn persist(&mut self) {
        let path = store::board_path(&self.cfg.storage_path, &self.board);
        if let Err(e) = store::save(&path, &self.tasks) {
            self.status = Some(format!("save failed: {e}"));
        }
    }

    fn save_cfg(&mut self) {
        if let Err(e) = config::save(&self.cfg) {
            self.status = Some(format!("config save failed: {e}"));
        }
    }

    /// The neon block-letter title, gradient top→bottom, with a subtitle line.
    fn banner(&self) -> Vec<Line<'static>> {
        let last = (BANNER.len() - 1).max(1) as f32;
        let mut lines: Vec<Line> = BANNER
            .iter()
            .enumerate()
            .map(|(i, art)| {
                let c = lerp_rgb(
                    self.theme.banner_top,
                    self.theme.banner_bottom,
                    i as f32 / last,
                );
                Line::from(Span::styled(
                    *art,
                    Style::default().fg(c).add_modifier(Modifier::BOLD),
                ))
            })
            .collect();
        lines.push(Line::from(
            format!("« zymosis — {} »", self.board)
                .fg(self.theme.accent)
                .add_modifier(Modifier::ITALIC),
        ));
        lines
    }

    fn draw(&self, f: &mut Frame) {
        let now = model::now();
        let th = self.ecfg().thresholds();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(BANNER.len() as u16 + 1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        f.render_widget(Paragraph::new(self.banner()), header);

        if matches!(self.mode, Mode::Help) {
            self.draw_help(f, body);
        } else if matches!(self.mode, Mode::Config | Mode::EditCfg(_)) {
            self.draw_config(f, body);
        } else if matches!(
            self.mode,
            Mode::Boards { .. }
                | Mode::AddBoard
                | Mode::RenameBoard { .. }
                | Mode::ConfirmDeleteBoard { .. }
        ) {
            self.draw_boards(f, body);
            if let Mode::ConfirmDeleteBoard { name } = &self.mode {
                self.draw_confirm(f, body, &format!("Delete board '{name}'?"));
            }
        } else {
            let rows = self.rows();
            let items: Vec<ListItem> = rows
                .iter()
                .map(|row| match *row {
                    Row::Task(ti) => {
                        let t = &self.tasks[ti];
                        let (mark, mut style) =
                            row_style(&self.theme, t.status(&th, now), t.age(now), &th, self.frame);
                        let (done, total) = t.progress();
                        let notes = t.notes.len();
                        let expand = if total > 0 || notes > 0 {
                            let caret = if self.expanded.contains(&t.id) {
                                "▾"
                            } else {
                                "▸"
                            };
                            let subs = if total > 0 {
                                format!(" [{done}/{total}]")
                            } else {
                                String::new()
                            };
                            let ns = if notes > 0 {
                                format!(" ✎{notes}")
                            } else {
                                String::new()
                            };
                            format!(" {caret}{subs}{ns}")
                        } else {
                            String::new()
                        };
                        let mark = if t.done { "✓" } else { mark };
                        if t.done {
                            style = style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
                        }
                        let mut spans =
                            vec![Span::styled(format!("{mark} {}{expand}", t.title), style)];
                        if !t.tags.is_empty() {
                            let chips = t
                                .tags
                                .iter()
                                .map(|x| format!("#{x}"))
                                .collect::<Vec<_>>()
                                .join(" ");
                            spans.push(Span::styled(
                                format!("  {chips}"),
                                Style::default().fg(self.theme.tag),
                            ));
                        }
                        ListItem::new(Line::from(spans))
                    }
                    Row::Sub(ti, si) => {
                        let s = &self.tasks[ti].subtasks[si];
                        let mark = if s.done { "✓" } else { "·" };
                        let mut style = Style::default().fg(self.theme.subtask);
                        if s.done {
                            style = style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
                        }
                        ListItem::new(Line::styled(format!("    ↳ {mark} {}", s.title), style))
                    }
                    Row::Note(ti, ni) => {
                        let n = &self.tasks[ti].notes[ni];
                        let style = Style::default()
                            .fg(self.theme.note)
                            .add_modifier(Modifier::ITALIC);
                        ListItem::new(Line::styled(format!("    ✎ {}", n.text), style))
                    }
                })
                .collect();

            let mut state = ListState::default();
            if !rows.is_empty() {
                state.select(Some(self.selected.min(rows.len() - 1)));
            }

            let count = self.visible().len();
            let title = match self.view {
                View::Active => format!(" active ({count}) "),
                View::Dormant => format!(" dormant ({count}) "),
                View::Done => format!(" done ({count}) "),
            };
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_symbol("▶ ")
                .highlight_style(
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::REVERSED),
                );
            f.render_stateful_widget(list, body, &mut state);
        }

        let hot = self.theme.hot;
        let w = footer.width as usize;
        // Prompt prefix for whichever mode is taking text input (None = not editing).
        let input_prefix = match self.mode {
            Mode::Add => Some("add> ".to_string()),
            Mode::AddSub(_) => Some("subtask> ".to_string()),
            Mode::AddNote(_) => Some("note> ".to_string()),
            Mode::Edit(_) => Some("edit> ".to_string()),
            Mode::EditSub(..) => Some("edit subtask> ".to_string()),
            Mode::EditNote(..) => Some("edit note> ".to_string()),
            Mode::EditTags(_) => Some("tags> ".to_string()),
            Mode::AddBoard => Some("new board> ".to_string()),
            Mode::RenameBoard { .. } => Some("rename> ".to_string()),
            Mode::Search => Some("/".to_string()),
            Mode::EditCfg(f) if self.status.is_none() => {
                Some(format!("{}> ", CFG_FIELDS[cfg_field_index(f)].1))
            }
            _ => None,
        };
        if let Some(prefix) = input_prefix {
            // Scroll horizontally so the cursor stays visible, then place the
            // real terminal cursor at its column.
            let (vis, col) = input_window(&prefix, &self.input, self.cursor, w);
            f.render_widget(Paragraph::new(Line::from(vis.fg(hot))), footer);
            f.set_cursor_position((footer.x + col as u16, footer.y));
            return;
        }
        let footer_line = match self.mode {
            Mode::EditCfg(_) => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => unreachable!("handled by input_label branch"),
            },
            Mode::Config => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from("↑↓ select · tab scope · enter edit · esc back".dim()),
            },
            Mode::Boards { .. } => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from("enter switch · a add · r rename · x delete · esc close".dim()),
            },
            Mode::ConfirmDeleteBoard { .. } => Line::from("y confirm · n/esc cancel".dim()),
            Mode::Help => Line::from("any key to close".dim()),
            Mode::Normal => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from(
                    "? help · a add · e edit · b boards · space done · tab view · q quit".dim(),
                ),
            },
            // Text-input modes returned early above with the cursor drawn.
            _ => unreachable!("input modes handled by input_label branch"),
        };
        f.render_widget(Paragraph::new(footer_line), footer);
    }

    fn draw_help(&self, f: &mut Frame, body: ratatui::layout::Rect) {
        let head =
            |s: &'static str| Line::from(s.fg(self.theme.accent).add_modifier(Modifier::BOLD));
        let key = |k: &'static str, desc: &'static str| {
            Line::from(vec![
                Span::styled(format!("  {k:10}"), Style::default().fg(self.theme.tag)),
                Span::raw(desc),
            ])
        };
        let lines = vec![
            head("navigation"),
            key("j/k ↑/↓", "move selection"),
            key("gg / G", "jump to top / bottom"),
            key("/", "search titles & tags (incremental)"),
            key("tab", "cycle active → dormant → done"),
            key("enter/→", "expand subtasks & notes"),
            Line::from(""),
            head("tasks"),
            key("a", "add task"),
            key("e", "edit title"),
            key("t", "edit tags"),
            key("space/d", "toggle done"),
            key("r", "revive (from dormant/done)"),
            key("x/del", "delete"),
            Line::from(""),
            head("subtasks & notes"),
            key("s", "add subtask"),
            key("n", "add note"),
            key("space", "toggle subtask done"),
            Line::from(""),
            head("editing"),
            key("←/→", "move cursor"),
            key("^a/^e", "cursor to start / end"),
            key("home/end", "cursor to start / end"),
            key("^u", "delete to line start"),
            key("del", "delete char at cursor"),
            Line::from(""),
            head("other"),
            key("y", "yank selected to clipboard"),
            key("b", "boards (switch / add)"),
            key("c", "config (tab: global/board scope)"),
            key("?", "this help"),
            key("q/esc", "quit"),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" help ")),
            body,
        );
    }

    fn draw_config(&self, f: &mut Frame, body: ratatui::layout::Rect) {
        let [head, list_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);
        let scope = match self.cfg_scope {
            Scope::Global => "scope: global".to_string(),
            Scope::Board => format!("scope: board {}", self.board),
        };
        f.render_widget(
            Paragraph::new(Line::from(scope.fg(self.theme.accent))),
            head,
        );

        let ecfg = self.ecfg();
        let over = self.cfg.boards.get(&self.board);
        let items: Vec<ListItem> = CFG_FIELDS
            .iter()
            .map(|(field, label)| {
                let (value, suffix) = match self.cfg_scope {
                    Scope::Global => (cfg_value(&self.cfg, *field), ""),
                    Scope::Board if *field == CfgField::TickFps => {
                        (cfg_value(&self.cfg, *field), " (global)")
                    }
                    Scope::Board => {
                        let set = over.is_some_and(|o| override_is_set(o, *field));
                        (
                            cfg_value(&ecfg, *field),
                            if set { " (override)" } else { "" },
                        )
                    }
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{label:14}"), Style::default().fg(self.theme.tag)),
                    Span::raw(value),
                    Span::styled(suffix, Style::default().fg(self.theme.dormant)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.cfg_sel.min(CFG_FIELDS.len() - 1)));
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" config "))
            .highlight_symbol("▶ ")
            .highlight_style(
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::REVERSED),
            );
        f.render_stateful_widget(list, list_area, &mut state);
    }

    fn draw_boards(&self, f: &mut Frame, body: ratatui::layout::Rect) {
        let names = self.board_names();
        let sel = match self.mode {
            Mode::Boards { sel, .. } => sel,
            _ => names.iter().position(|n| *n == self.board).unwrap_or(0),
        };
        let items: Vec<ListItem> = names
            .iter()
            .map(|n| {
                let mark = if *n == self.board { "* " } else { "  " };
                ListItem::new(Line::from(format!("{mark}{n}")))
            })
            .collect();
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(sel.min(items.len() - 1)));
        }
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" boards "))
            .highlight_symbol("▶ ")
            .highlight_style(
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::REVERSED),
            );
        f.render_stateful_widget(list, body, &mut state);
    }

    /// Centered yes/no confirmation popup over `area`.
    fn draw_confirm(&self, f: &mut Frame, area: Rect, question: &str) {
        let w = (question.chars().count() as u16 + 4).clamp(24, area.width.max(1));
        let h = 4.min(area.height.max(1));
        let rect = Rect {
            x: area.x + area.width.saturating_sub(w) / 2,
            y: area.y + area.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        };
        f.render_widget(Clear, rect);
        let body = vec![
            Line::from(question.to_string()),
            Line::from("y: yes   n: no".fg(self.theme.dormant)),
        ];
        f.render_widget(
            Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" confirm ")
                    .border_style(Style::default().fg(self.theme.hot)),
            ),
            rect,
        );
    }
}

/// Keep the trailing `width` chars so the end of a long input stays visible.
fn tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    s.chars().skip(n - width).collect()
}

/// Byte offset of char index `i` (or the string's end when `i` is past it).
fn byte_at(s: &str, i: usize) -> usize {
    s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len())
}

/// Window a `prefix + input` line to `width` columns, scrolling horizontally so
/// the cursor stays visible. Returns the visible slice and the cursor's column
/// within it. `cursor` is a char index into `input`.
fn input_window(prefix: &str, input: &str, cursor: usize, width: usize) -> (String, usize) {
    let width = width.max(1);
    let full: Vec<char> = prefix.chars().chain(input.chars()).collect();
    let cur = prefix.chars().count() + cursor.min(input.chars().count());
    let start = cur.saturating_sub(width - 1);
    let vis: String = full.iter().skip(start).take(width).collect();
    (vis, cur - start)
}

/// Copy `text` to the system clipboard via the OSC 52 terminal escape. Portable
/// (works over SSH, no X/Wayland/pbcopy fork) and dependency-free.
// ponytail: OSC 52 needs terminal support (kitty/iterm2/wezterm/tmux, etc.);
// add an arboard fallback only if a target terminal turns out not to honor it.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Whether `name` may be deleted, mirroring the CLI's guards: never the active
/// board, never the last remaining one.
fn board_deletable(name: &str, active: &str, total: usize) -> Result<(), &'static str> {
    if name == active {
        Err("cannot delete the active board")
    } else if total <= 1 {
        Err("cannot delete the only board")
    } else {
        Ok(())
    }
}

fn move_selection(cur: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as i32 + delta).clamp(0, len as i32 - 1) as usize
}

fn sort_rank(s: Status) -> u8 {
    match s {
        Status::Hot => 0,
        Status::Bubbling => 1,
        Status::Decaying => 2,
        Status::Dormant => 3,
    }
}

// --- palette + animation (pure, testable) ---

/// All theme-able colors in one place. Rendering reads only from a `Theme`, so a
/// new palette is a new `const Theme` + a name in `Theme::named` — the groundwork
/// for user-configurable themes.
// ponytail: built-in named themes only; add per-field RGB overrides in the TOML
// (a `[theme]` table merged over the named base) if anyone wants custom colors.
#[derive(Clone, Copy)]
struct Theme {
    accent: Color,
    hot: Color,
    bubble: Color,
    decay_fresh: Color,
    decay_stale: Color,
    dormant: Color,
    subtask: Color,
    note: Color,
    tag: Color,
    banner_top: Color,
    banner_bottom: Color,
}

const THEME_NAMES: [&str; 6] = [
    "neon_purple",
    "neon_teal",
    "catppuccin_mocha",
    "catppuccin_macchiato",
    "catppuccin_frappe",
    "catppuccin_latte",
];

/// Default: loud purple-forward neon-punk.
const NEON_PURPLE: Theme = Theme {
    accent: Color::Rgb(187, 92, 255),
    hot: Color::Rgb(255, 45, 149),
    bubble: Color::Rgb(150, 120, 255),
    decay_fresh: Color::Rgb(200, 170, 235),
    decay_stale: Color::Rgb(110, 95, 135),
    dormant: Color::Rgb(90, 80, 110),
    subtask: Color::Rgb(160, 150, 190),
    note: Color::Rgb(150, 190, 150),
    tag: Color::Rgb(200, 140, 255),
    banner_top: Color::Rgb(255, 60, 200),
    banner_bottom: Color::Rgb(120, 90, 255),
};

/// The original teal palette, kept selectable to prove themes are swappable.
const NEON_TEAL: Theme = Theme {
    accent: Color::Rgb(0, 255, 213),
    hot: Color::Rgb(255, 60, 172),
    bubble: Color::Rgb(90, 230, 255),
    decay_fresh: Color::Rgb(150, 200, 210),
    decay_stale: Color::Rgb(95, 95, 120),
    dormant: Color::Rgb(80, 80, 105),
    subtask: Color::Rgb(140, 150, 170),
    note: Color::Rgb(150, 170, 140),
    tag: Color::Rgb(180, 140, 255),
    banner_top: Color::Rgb(0, 255, 213),
    banner_bottom: Color::Rgb(90, 230, 255),
};

/// Catppuccin Mocha (the flagship dark flavor). Field mapping: mauve accent,
/// red for hot, blue for bubbling, green notes, lavender tags.
const CATPPUCCIN_MOCHA: Theme = Theme {
    accent: Color::Rgb(203, 166, 247),
    hot: Color::Rgb(243, 139, 168),
    bubble: Color::Rgb(137, 180, 250),
    decay_fresh: Color::Rgb(205, 214, 244),
    decay_stale: Color::Rgb(108, 112, 134),
    dormant: Color::Rgb(88, 91, 112),
    subtask: Color::Rgb(166, 173, 200),
    note: Color::Rgb(166, 227, 161),
    tag: Color::Rgb(180, 190, 254),
    banner_top: Color::Rgb(245, 194, 231),
    banner_bottom: Color::Rgb(203, 166, 247),
};

/// Catppuccin Macchiato.
const CATPPUCCIN_MACCHIATO: Theme = Theme {
    accent: Color::Rgb(198, 160, 246),
    hot: Color::Rgb(237, 135, 150),
    bubble: Color::Rgb(138, 173, 244),
    decay_fresh: Color::Rgb(202, 211, 245),
    decay_stale: Color::Rgb(110, 115, 141),
    dormant: Color::Rgb(91, 96, 120),
    subtask: Color::Rgb(165, 173, 203),
    note: Color::Rgb(166, 218, 149),
    tag: Color::Rgb(183, 189, 248),
    banner_top: Color::Rgb(245, 189, 230),
    banner_bottom: Color::Rgb(198, 160, 246),
};

/// Catppuccin Frappé.
const CATPPUCCIN_FRAPPE: Theme = Theme {
    accent: Color::Rgb(202, 158, 230),
    hot: Color::Rgb(231, 130, 132),
    bubble: Color::Rgb(140, 170, 238),
    decay_fresh: Color::Rgb(198, 208, 245),
    decay_stale: Color::Rgb(115, 121, 148),
    dormant: Color::Rgb(98, 104, 128),
    subtask: Color::Rgb(165, 173, 206),
    note: Color::Rgb(166, 209, 137),
    tag: Color::Rgb(186, 187, 241),
    banner_top: Color::Rgb(244, 184, 228),
    banner_bottom: Color::Rgb(202, 158, 230),
};

/// Catppuccin Latte (the light flavor). Darker foregrounds since the base is light.
const CATPPUCCIN_LATTE: Theme = Theme {
    accent: Color::Rgb(136, 57, 239),
    hot: Color::Rgb(210, 15, 57),
    bubble: Color::Rgb(30, 102, 245),
    decay_fresh: Color::Rgb(76, 79, 105),
    decay_stale: Color::Rgb(156, 160, 176),
    dormant: Color::Rgb(188, 192, 204),
    subtask: Color::Rgb(92, 95, 119),
    note: Color::Rgb(64, 160, 43),
    tag: Color::Rgb(114, 135, 253),
    banner_top: Color::Rgb(234, 118, 203),
    banner_bottom: Color::Rgb(136, 57, 239),
};

impl Theme {
    /// Resolve a config theme name; unknown names fall back to the default.
    fn named(name: &str) -> Theme {
        match name {
            "neon_teal" => NEON_TEAL,
            "catppuccin_mocha" => CATPPUCCIN_MOCHA,
            "catppuccin_macchiato" => CATPPUCCIN_MACCHIATO,
            "catppuccin_frappe" => CATPPUCCIN_FRAPPE,
            "catppuccin_latte" => CATPPUCCIN_LATTE,
            _ => NEON_PURPLE,
        }
    }
}

const BUBBLES: [&str; 4] = ["·", "∘", "○", "°"];

/// Block-letter "ZYM" banner; each row gets a top→bottom gradient at draw time.
const BANNER: [&str; 5] = [
    "███████  ██   ██  ███    ███",
    "    ███   ██ ██   ████  ████",
    "   ███     ███    ██ ████ ██",
    "  ███      ██     ██  ██  ██",
    "███████    ██     ██      ██",
];

fn to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (255, 255, 255),
    }
}

fn scale_rgb(c: Color, f: f32) -> Color {
    let (r, g, b) = to_rgb(c);
    let s = |v: u8| (v as f32 * f).clamp(0.0, 255.0) as u8;
    Color::Rgb(s(r), s(g), s(b))
}

fn lerp_rgb(a: Color, b: Color, t: f32) -> Color {
    let (ar, ag, ab) = to_rgb(a);
    let (br, bg, bb) = to_rgb(b);
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color::Rgb(l(ar, br), l(ag, bg), l(ab, bb))
}

fn breathe(base: Color, frame: u64) -> Color {
    const PERIOD: u64 = 12;
    let phase = frame % (2 * PERIOD);
    let tri = if phase < PERIOD {
        phase as f32 / PERIOD as f32
    } else {
        (2 * PERIOD - phase) as f32 / PERIOD as f32
    };
    scale_rgb(base, 0.75 + 0.25 * tri)
}

fn bubble_glyph(frame: u64) -> &'static str {
    BUBBLES[((frame / 2) % BUBBLES.len() as u64) as usize]
}

fn decay_color(theme: &Theme, age: Duration, th: &Thresholds) -> Color {
    let start = th.hot_window.as_secs() as f32;
    let end = th.dormant_after.as_secs() as f32;
    let t = if end > start {
        (age.as_secs() as f32 - start) / (end - start)
    } else {
        0.0
    };
    lerp_rgb(theme.decay_fresh, theme.decay_stale, t)
}

fn row_style(
    theme: &Theme,
    st: Status,
    age: Duration,
    th: &Thresholds,
    frame: u64,
) -> (&'static str, Style) {
    match st {
        Status::Hot => (
            "·",
            Style::default()
                .fg(breathe(theme.hot, frame))
                .add_modifier(Modifier::BOLD),
        ),
        Status::Bubbling => (
            bubble_glyph(frame),
            Style::default()
                .fg(breathe(theme.bubble, frame))
                .add_modifier(Modifier::BOLD),
        ),
        Status::Decaying => ("·", Style::default().fg(decay_color(theme, age, th))),
        Status::Dormant => (
            "·",
            Style::default()
                .fg(theme.dormant)
                .add_modifier(Modifier::DIM),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SubTask;

    fn app_with(n: usize) -> App {
        let tasks = (0..n)
            .map(|i| Task::new(i as u64 + 1, format!("t{i}")))
            .collect();
        App::new(Config::default(), tasks)
    }

    fn th() -> Thresholds {
        Config::default().thresholds()
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 test vectors, incl. the three padding cases.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn tail_keeps_trailing_chars() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("abcdef", 3), "def");
        // char-aware, not byte-aware
        assert_eq!(tail("aééé", 2), "éé");
    }

    #[test]
    fn input_window_scrolls_to_keep_cursor_visible() {
        // Fits: whole line shown, cursor where it sits.
        assert_eq!(input_window("add> ", "hi", 2, 20), ("add> hi".into(), 7));
        // Cursor at the start of a long input: window anchored at the start.
        let (vis, col) = input_window("e> ", "0123456789", 0, 6);
        assert_eq!((vis.as_str(), col), ("e> 012", 3));
        // Cursor at the end: window scrolls right, cursor on the last column.
        let (vis, col) = input_window("e> ", "0123456789", 10, 6);
        // Cursor sits one past the last char, so width-1 chars are shown.
        assert_eq!(col, 5);
        assert!(vis.ends_with('9') && vis.chars().count() == 5);
        // byte_at is char-aware.
        assert_eq!(byte_at("aé", 1), 1);
        assert_eq!(byte_at("aé", 2), 3);
    }

    // Stagger last_updated so ties don't leave the sort order ambiguous.
    fn stagger(app: &mut App) {
        let now = model::now();
        let n = app.tasks.len() as u64;
        for (i, t) in app.tasks.iter_mut().enumerate() {
            t.last_updated = now - (n - i as u64); // t0 oldest → bottom
        }
    }

    #[test]
    fn selection_follows_task_it_acted_on() {
        let mut app = app_with(3);
        stagger(&mut app); // t2 newest at top, t0 oldest at bottom
        let id = app.tasks[0].id; // the bottom task

        // Editing touches it → floats to the top; selection must follow.
        app.apply_edit(id, "edited".into());
        assert_eq!(app.selected, 0);
        assert_eq!(app.selected_task().map(|ti| app.tasks[ti].id), Some(id));

        // Same for adding a subtask to a task that isn't currently selected.
        app.selected = app.rows().len() - 1; // move away, to the bottom
        let other = app.tasks.iter().find(|t| t.id != id).unwrap().id;
        app.add_subtask(other, "sub".into());
        assert_eq!(app.selected_task().map(|ti| app.tasks[ti].id), Some(other));
    }

    #[test]
    fn find_match_hits_title_and_tags_case_insensitively() {
        let mut app = app_with(0);
        app.tasks.push(Task::new(1, "Buy Milk"));
        app.tasks.push(Task::new(2, "walk dog"));
        let mut tagged = Task::new(3, "call mom");
        tagged.add_tag("Urgent");
        app.tasks.push(tagged);
        stagger(&mut app);

        let i = app.find_match("DOG").expect("title match");
        assert!(matches!(app.rows()[i], Row::Task(ti) if app.tasks[ti].id == 2));
        let i = app.find_match("urgent").expect("tag match");
        assert!(matches!(app.rows()[i], Row::Task(ti) if app.tasks[ti].id == 3));
        assert!(app.find_match("zzz").is_none());
        assert!(app.find_match("   ").is_none());
    }

    #[test]
    fn gg_and_capital_g_jump_to_top_and_bottom() {
        let mut app = app_with(4);
        stagger(&mut app);
        let bottom = app.rows().len() - 1;
        app.selected = 1;

        app.on_normal_key(KeyCode::Char('G'));
        assert_eq!(app.selected, bottom);

        app.on_normal_key(KeyCode::Char('g')); // arms
        assert!(app.g_pending);
        app.on_normal_key(KeyCode::Char('g')); // fires → top
        assert_eq!(app.selected, 0);
        assert!(!app.g_pending);

        // A lone `g` then any other key cancels the pending gg.
        app.on_normal_key(KeyCode::Char('g'));
        app.on_normal_key(KeyCode::Char('j'));
        assert!(!app.g_pending);
    }

    #[test]
    fn yank_sets_status_for_each_row_kind() {
        let mut app = app_with(1);
        app.tasks[0].subtasks = vec![SubTask {
            title: "sub".into(),
            done: false,
        }];
        app.add_note(app.tasks[0].id, "a note".into()); // auto-expands
        app.selected = 0;
        app.yank_selected();
        assert_eq!(app.status.as_deref(), Some("yanked: t0"));
        app.selected = 1; // subtask row
        app.yank_selected();
        assert_eq!(app.status.as_deref(), Some("yanked: sub"));
        app.selected = 2; // note row
        app.yank_selected();
        assert_eq!(app.status.as_deref(), Some("yanked: a note"));
    }

    #[test]
    fn board_deletable_guards_active_and_last() {
        assert!(board_deletable("work", "default", 2).is_ok());
        assert!(board_deletable("default", "default", 2).is_err()); // active
        assert!(board_deletable("only", "default", 1).is_err()); // last board
    }

    #[test]
    fn selection_clamps_to_bounds() {
        assert_eq!(move_selection(0, 0, 1), 0);
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(2, 3, 1), 2);
        assert_eq!(move_selection(1, 3, 1), 2);
        assert_eq!(move_selection(1, 3, -1), 0);
    }

    #[test]
    fn add_assigns_next_id() {
        let mut app = app_with(2);
        app.add_task("new".into());
        assert_eq!(app.tasks.len(), 3);
        assert_eq!(app.tasks.last().unwrap().id, 3);
    }

    #[test]
    fn toggle_task_flips_done() {
        let mut app = app_with(1);
        assert!(app.toggle_selected());
        assert!(app.tasks[0].done);
    }

    #[test]
    fn remove_clamps_selection() {
        let mut app = app_with(3);
        app.selected = 2;
        assert!(app.remove_selected());
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn mutations_on_empty_are_noops() {
        let mut app = app_with(0);
        assert!(!app.toggle_selected());
        assert!(!app.remove_selected());
        assert!(!app.revive_selected());
    }

    #[test]
    fn expand_reveals_subtask_rows_and_toggles_them() {
        let mut app = app_with(1);
        app.tasks[0].subtasks = vec![
            SubTask {
                title: "a".into(),
                done: false,
            },
            SubTask {
                title: "b".into(),
                done: false,
            },
        ];
        assert_eq!(app.rows().len(), 1); // collapsed: just the task
        app.selected = 0;
        app.toggle_expand();
        assert_eq!(app.rows().len(), 3); // task + 2 subtasks

        // move to first subtask row and toggle it
        app.selected = 1;
        assert!(app.toggle_selected());
        assert!(app.tasks[0].subtasks[0].done);
        assert!(!app.tasks[0].subtasks[1].done);
    }

    #[test]
    fn add_subtask_appends_and_expands() {
        let mut app = app_with(1);
        let id = app.tasks[0].id;
        app.add_subtask(id, "first".into());
        assert_eq!(app.tasks[0].subtasks.len(), 1);
        assert!(app.expanded.contains(&id)); // auto-revealed
        assert_eq!(app.rows().len(), 2); // task + 1 subtask
    }

    #[test]
    fn edit_targets_the_selected_subtask_and_note_row() {
        let mut app = app_with(1);
        let id = app.tasks[0].id;
        app.add_subtask(id, "old sub".into());
        app.add_note(id, "old note".into());

        app.apply_edit_sub(id, 0, "new sub".into());
        assert_eq!(app.tasks[0].subtasks[0].title, "new sub");
        app.apply_edit_note(id, 0, "new note".into());
        assert_eq!(app.tasks[0].notes[0].text, "new note");
        // task title untouched by subtask/note edits
        assert_eq!(app.tasks[0].title, "t0");
    }

    #[test]
    fn remove_on_subtask_row_deletes_only_the_subtask() {
        let mut app = app_with(1);
        app.tasks[0].subtasks = vec![
            SubTask {
                title: "a".into(),
                done: false,
            },
            SubTask {
                title: "b".into(),
                done: false,
            },
        ];
        app.toggle_expand(); // selected is task row 0
        app.selected = 1; // first subtask row
        assert!(app.remove_selected());
        assert_eq!(app.tasks.len(), 1); // task survives
        assert_eq!(app.tasks[0].subtasks.len(), 1);
        assert_eq!(app.tasks[0].subtasks[0].title, "b");
    }

    #[test]
    fn add_note_appends_expands_and_note_row_is_removable() {
        let mut app = app_with(1);
        let id = app.tasks[0].id;
        app.add_note(id, "  weigh the tradeoff  ".into());
        assert_eq!(app.tasks[0].notes.len(), 1);
        assert_eq!(app.tasks[0].notes[0].text, "weigh the tradeoff"); // trimmed
        assert!(app.expanded.contains(&id)); // auto-revealed
        assert_eq!(app.rows().len(), 2); // task + 1 note row

        // notes have no done state: space is a no-op on a note row
        app.selected = 1;
        assert!(!app.toggle_selected());
        // but the note row can be deleted
        assert!(app.remove_selected());
        assert!(app.tasks[0].notes.is_empty());
        assert_eq!(app.tasks.len(), 1); // task survives
    }

    #[test]
    fn edit_tags_sets_normalizes_dedups_and_clears() {
        let mut app = app_with(1);
        let id = app.tasks[0].id;
        app.apply_tags(id, "  Perf  #Monitoring perf ".into());
        assert_eq!(app.tasks[0].tags, vec!["perf", "monitoring"]); // trimmed, lowercased, deduped, # stripped
        app.apply_tags(id, String::new());
        assert!(app.tasks[0].tags.is_empty()); // empty input clears
    }

    #[test]
    fn sort_priority_hot_bubbling_first() {
        assert!(sort_rank(Status::Hot) < sort_rank(Status::Bubbling));
        assert!(sort_rank(Status::Bubbling) < sort_rank(Status::Decaying));
        assert!(sort_rank(Status::Decaying) < sort_rank(Status::Dormant));
    }

    #[test]
    fn bubble_glyph_cycles() {
        assert_eq!(bubble_glyph(0), "·");
        assert_eq!(bubble_glyph(0), bubble_glyph(2 * BUBBLES.len() as u64));
    }

    #[test]
    fn decay_color_ramps_between_endpoints() {
        let t = th();
        let tm = NEON_PURPLE;
        assert_eq!(decay_color(&tm, t.hot_window, &t), tm.decay_fresh);
        assert_eq!(decay_color(&tm, t.dormant_after, &t), tm.decay_stale);
        let mid = decay_color(&tm, (t.hot_window + t.dormant_after) / 2, &t);
        assert_ne!(mid, tm.decay_fresh);
        assert_ne!(mid, tm.decay_stale);
    }

    #[test]
    fn breathe_never_brighter_than_base() {
        for frame in 0..48u64 {
            let (r, g, b) = to_rgb(breathe(NEON_PURPLE.hot, frame));
            let (br, bg, bb) = to_rgb(NEON_PURPLE.hot);
            assert!(r <= br && g <= bg && b <= bb);
        }
    }

    #[test]
    fn theme_named_falls_back_to_default_on_unknown() {
        let default = Theme::named("neon_purple");
        assert_eq!(to_rgb(Theme::named("bogus").accent), to_rgb(default.accent));
        assert_ne!(
            to_rgb(Theme::named("neon_teal").accent),
            to_rgb(default.accent)
        );
    }

    fn global(cfg: &Config, f: CfgField, input: &str) -> Result<Config, String> {
        apply_cfg(cfg, "default", Scope::Global, f, input)
    }

    #[test]
    fn apply_cfg_theme_validates_name_and_switches() {
        let cfg = Config::default();
        assert_eq!(
            global(&cfg, CfgField::Theme, "neon_teal").unwrap().theme,
            "neon_teal"
        );
        assert!(global(&cfg, CfgField::Theme, "nope").is_err());
    }

    #[test]
    fn apply_cfg_parses_validates_and_rejects() {
        let cfg = Config::default();
        // valid span edit round-trips through the human format
        let next = global(&cfg, CfgField::HotWindow, "1d").unwrap();
        assert_eq!(next.hot_window.as_human(), "1d");
        // tick_fps parses as a number
        assert_eq!(global(&cfg, CfgField::TickFps, "30").unwrap().tick_fps, 30);
        // garbage span and number are rejected
        assert!(global(&cfg, CfgField::HotWindow, "nope").is_err());
        assert!(global(&cfg, CfgField::TickFps, "x").is_err());
        // tick_fps must be >= 1, and hot_window must stay <= dormant_after
        assert!(global(&cfg, CfgField::TickFps, "0").is_err());
        assert!(global(&cfg, CfgField::HotWindow, "999d").is_err());
    }

    #[test]
    fn apply_cfg_board_scope_sets_clears_and_validates() {
        let cfg = Config::default();
        // Board-scope edit records an override, leaving the global untouched.
        let c = apply_cfg(&cfg, "work", Scope::Board, CfgField::HotWindow, "1d").unwrap();
        assert_eq!(
            c.boards["work"].hot_window,
            Some(CfgSpan::parse("1d").unwrap())
        );
        assert_eq!(c.hot_window, cfg.hot_window);
        // Empty input clears the override and drops the now-empty entry.
        let c = apply_cfg(&c, "work", Scope::Board, CfgField::HotWindow, "").unwrap();
        assert!(!c.boards.contains_key("work"));
        // A board override that breaks hot_window <= dormant_after is rejected.
        assert!(apply_cfg(&cfg, "work", Scope::Board, CfgField::HotWindow, "999d").is_err());
        // tick_fps is not editable per-board.
        assert!(apply_cfg(&cfg, "work", Scope::Board, CfgField::TickFps, "30").is_err());
    }

    #[test]
    fn views_partition_active_dormant_and_done() {
        let mut app = app_with(0);
        app.tasks.push(Task::new(1, "fresh")); // active
        let mut dorm = Task::new(2, "dorm");
        dorm.last_updated = model::now() - 16 * 86_400; // dormant band
        app.tasks.push(dorm);
        let mut done = Task::new(3, "done");
        done.done = true;
        app.tasks.push(done);

        app.view = View::Active;
        assert_eq!(app.visible(), vec![0]);
        app.view = View::Dormant;
        assert_eq!(app.visible(), vec![1]);
        app.view = View::Done;
        assert_eq!(app.visible(), vec![2]);
    }

    #[test]
    fn delete_from_done_view_retires_task() {
        let mut app = app_with(0);
        let mut done = Task::new(1, "done");
        done.done = true;
        app.tasks.push(done);
        app.view = View::Done;
        assert!(app.remove_selected());
        assert!(app.tasks.is_empty());
    }

    #[test]
    fn revive_clears_done_and_returns_to_active() {
        let mut app = app_with(0);
        let mut done = Task::new(1, "done");
        done.done = true;
        app.tasks.push(done);
        app.view = View::Done;
        assert!(app.revive_selected());
        assert!(!app.tasks[0].done);
        app.view = View::Active;
        assert_eq!(app.visible(), vec![0]);
    }
}
