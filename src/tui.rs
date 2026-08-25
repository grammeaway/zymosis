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

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{self, Config, Span as CfgSpan};
use crate::model::{self, Status, SubTask, Task, Thresholds};
use crate::store;

pub fn run() -> Result<(), String> {
    let cfg = config::load()?;
    let tasks = store::load(&cfg.storage_path).map_err(|e| e.to_string())?;
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
    AddSub(u64),
    AddNote(u64),
    EditTags(u64),
    Config,
    EditCfg(CfgField),
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

/// Apply one field edit to a copy of the config, parsing + validating. Pure so
/// the parse/validate branch is testable without a terminal.
fn apply_cfg(cfg: &Config, f: CfgField, input: &str) -> Result<Config, String> {
    let mut next = cfg.clone();
    match f {
        CfgField::HotWindow => next.hot_window = CfgSpan::parse(input)?,
        CfgField::DormantAfter => next.dormant_after = CfgSpan::parse(input)?,
        CfgField::BubbleAfter => next.bubble_after = CfgSpan::parse(input)?,
        CfgField::TickFps => {
            next.tick_fps = input
                .trim()
                .parse()
                .map_err(|_| format!("bad number '{}'", input.trim()))?
        }
        CfgField::Theme => {
            let name = input.trim();
            if !THEME_NAMES.contains(&name) {
                return Err(format!(
                    "unknown theme '{name}' (try: {})",
                    THEME_NAMES.join(", ")
                ));
            }
            next.theme = name.to_string();
        }
    }
    next.validate()?;
    Ok(next)
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
    theme: Theme,
    tasks: Vec<Task>,
    selected: usize,
    mode: Mode,
    input: String,
    status: Option<String>,
    view: View,
    expanded: HashSet<u64>,
    cfg_sel: usize,
    frame: u64,
    quit: bool,
}

impl App {
    fn new(cfg: Config, tasks: Vec<Task>) -> Self {
        let theme = Theme::named(&cfg.theme);
        Self {
            cfg,
            theme,
            tasks,
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            view: View::Active,
            expanded: HashSet::new(),
            cfg_sel: 0,
            frame: 0,
            quit: false,
        }
    }

    /// Task indices belonging to the current view, ordered by lifecycle
    /// priority (Hot/Bubbling rise) and recency within a band. Done tasks live
    /// only in the Done view; Active and Dormant split the rest by status.
    fn visible(&self) -> Vec<usize> {
        let now = model::now();
        let th = self.cfg.thresholds();
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
        let th = self.cfg.thresholds();
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
                            _ => self.on_input_key(k.code),
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
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
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
                self.input.clear();
                self.mode = Mode::Add;
            }
            KeyCode::Char('s') => {
                if let Some(ti) = self.row_task() {
                    self.input.clear();
                    self.mode = Mode::AddSub(self.tasks[ti].id);
                }
            }
            KeyCode::Char('n') => {
                if let Some(ti) = self.row_task() {
                    self.input.clear();
                    self.mode = Mode::AddNote(self.tasks[ti].id);
                }
            }
            KeyCode::Char('e') => {
                if let Some(i) = self.selected_task() {
                    self.input = self.tasks[i].title.clone();
                    self.mode = Mode::Edit(self.tasks[i].id);
                }
            }
            KeyCode::Char('t') => {
                if let Some(i) = self.selected_task() {
                    self.input = self.tasks[i].tags.join(" ");
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
                self.mode = Mode::Config;
            }
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
            KeyCode::Down | KeyCode::Char('j') => {
                self.cfg_sel = move_selection(self.cfg_sel, CFG_FIELDS.len(), 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.cfg_sel = move_selection(self.cfg_sel, CFG_FIELDS.len(), -1)
            }
            KeyCode::Enter => {
                let f = CFG_FIELDS[self.cfg_sel].0;
                self.input = cfg_value(&self.cfg, f);
                self.mode = Mode::EditCfg(f);
            }
            _ => {}
        }
    }

    fn on_input_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = if matches!(self.mode, Mode::EditCfg(_)) {
                    Mode::Config
                } else {
                    Mode::Normal
                };
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            // Config edits parse/validate and stay on the config screen; a bad
            // value shows an error and keeps the input for a retry.
            KeyCode::Enter if matches!(self.mode, Mode::EditCfg(_)) => {
                let Mode::EditCfg(f) = self.mode else {
                    unreachable!()
                };
                match apply_cfg(&self.cfg, f, &self.input) {
                    Ok(next) => {
                        self.cfg = next;
                        self.theme = Theme::named(&self.cfg.theme); // live theme switch
                        self.save_cfg();
                        self.mode = Mode::Config;
                        self.input.clear();
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
                } else if !title.is_empty() {
                    match self.mode {
                        Mode::Add => self.add_task(title),
                        Mode::Edit(id) => self.apply_edit(id, title),
                        Mode::AddSub(id) => self.add_subtask(id, title),
                        Mode::AddNote(id) => self.add_note(id, title),
                        Mode::Normal | Mode::Help | Mode::Config | Mode::EditCfg(_) | Mode::EditTags(_) => {}
                    }
                    self.persist();
                }
                self.mode = Mode::Normal;
                self.input.clear();
            }
            _ => {}
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
        self.selected = 0;
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
        self.selected = 0;
    }

    fn add_subtask(&mut self, id: u64, title: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.subtasks.push(SubTask { title, done: false });
            t.touch();
        }
        self.expanded.insert(id); // reveal what was just added
    }

    fn add_note(&mut self, id: u64, text: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.add_note(&text);
            t.touch();
        }
        self.expanded.insert(id); // reveal what was just added
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
                self.selected = 0;
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
        if let Err(e) = store::save(&self.cfg.storage_path, &self.tasks) {
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
            "« zymosis — fermenting todo »"
                .fg(self.theme.accent)
                .add_modifier(Modifier::ITALIC),
        ));
        lines
    }

    fn draw(&self, f: &mut Frame) {
        let now = model::now();
        let th = self.cfg.thresholds();
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
        // Tail long inputs so the cursor end stays on screen (footer is one line).
        let w = footer.width as usize;
        let prompt = |label: &str, s: &str| Line::from(tail(&format!("{label}> {s}"), w).fg(hot));
        let footer_line = match self.mode {
            Mode::Add => prompt("add", &self.input),
            Mode::AddSub(_) => prompt("subtask", &self.input),
            Mode::AddNote(_) => prompt("note", &self.input),
            Mode::Edit(_) => prompt("edit", &self.input),
            Mode::EditTags(_) => prompt("tags", &self.input),
            Mode::EditCfg(f) => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => prompt(CFG_FIELDS[cfg_field_index(f)].1, &self.input),
            },
            Mode::Config => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from("↑↓ select · enter edit · esc back".dim()),
            },
            Mode::Help => Line::from("any key to close".dim()),
            Mode::Normal => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from(
                    "? help · a add · e edit · y yank · space done · tab view · q quit".dim(),
                ),
            },
        };
        f.render_widget(Paragraph::new(footer_line), footer);
    }

    fn draw_help(&self, f: &mut Frame, body: ratatui::layout::Rect) {
        let head = |s: &'static str| Line::from(s.fg(self.theme.accent).add_modifier(Modifier::BOLD));
        let key = |k: &'static str, desc: &'static str| {
            Line::from(vec![
                Span::styled(format!("  {k:10}"), Style::default().fg(self.theme.tag)),
                Span::raw(desc),
            ])
        };
        let lines = vec![
            head("navigation"),
            key("j/k ↑/↓", "move selection"),
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
            head("other"),
            key("y", "yank selected to clipboard"),
            key("c", "config"),
            key("?", "this help"),
            key("q/esc", "quit"),
        ];
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" help ")),
            body,
        );
    }

    fn draw_config(&self, f: &mut Frame, body: ratatui::layout::Rect) {
        let items: Vec<ListItem> = CFG_FIELDS
            .iter()
            .map(|(field, label)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{label:14}"), Style::default().fg(self.theme.tag)),
                    Span::raw(cfg_value(&self.cfg, *field)),
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
        f.render_stateful_widget(list, body, &mut state);
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
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
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
    fn yank_sets_status_for_each_row_kind() {
        let mut app = app_with(1);
        app.tasks[0].subtasks = vec![SubTask { title: "sub".into(), done: false }];
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

    #[test]
    fn apply_cfg_theme_validates_name_and_switches() {
        let cfg = Config::default();
        assert_eq!(
            apply_cfg(&cfg, CfgField::Theme, "neon_teal").unwrap().theme,
            "neon_teal"
        );
        assert!(apply_cfg(&cfg, CfgField::Theme, "nope").is_err());
    }

    #[test]
    fn apply_cfg_parses_validates_and_rejects() {
        let cfg = Config::default();
        // valid span edit round-trips through the human format
        let next = apply_cfg(&cfg, CfgField::HotWindow, "1d").unwrap();
        assert_eq!(next.hot_window.as_human(), "1d");
        // tick_fps parses as a number
        assert_eq!(
            apply_cfg(&cfg, CfgField::TickFps, "30").unwrap().tick_fps,
            30
        );
        // garbage span and number are rejected
        assert!(apply_cfg(&cfg, CfgField::HotWindow, "nope").is_err());
        assert!(apply_cfg(&cfg, CfgField::TickFps, "x").is_err());
        // tick_fps must be >= 1, and hot_window must stay <= dormant_after
        assert!(apply_cfg(&cfg, CfgField::TickFps, "0").is_err());
        assert!(apply_cfg(&cfg, CfgField::HotWindow, "999d").is_err());
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
