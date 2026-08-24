//! Interactive TUI.
//! Slice 1: read-only list, status styling, navigation.
//! Slice 2: add/edit/done/revive/delete with atomic save-on-change.
//!
//! Terminal setup/teardown goes through `ratatui::try_init`/`try_restore`, which
//! install a panic hook that restores the terminal — so a crash never leaves the
//! user's shell in raw mode, and a missing TTY is a clean error, not a panic.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{self, Config};
use crate::model::{self, Status, Task};
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

/// Editing modes. `Add` builds a new task; `Edit(id)` retitles an existing one.
enum Mode {
    Normal,
    Add,
    Edit(u64),
}

struct App {
    cfg: Config,
    tasks: Vec<Task>,
    selected: usize,
    mode: Mode,
    input: String,
    status: Option<String>,
    quit: bool,
}

impl App {
    fn new(cfg: Config, tasks: Vec<Task>) -> Self {
        Self {
            cfg,
            tasks,
            selected: 0,
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            quit: false,
        }
    }

    /// Display order: most-recently-updated first.
    fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.tasks.len()).collect();
        idx.sort_by(|&a, &b| self.tasks[b].last_updated.cmp(&self.tasks[a].last_updated));
        idx
    }

    /// Index into `self.tasks` of the currently-selected row, if any.
    fn selected_task(&self) -> Option<usize> {
        self.order().get(self.selected).copied()
    }

    fn run(&mut self, term: &mut DefaultTerminal) -> Result<(), String> {
        while !self.quit {
            term.draw(|f| self.draw(f)).map_err(|e| e.to_string())?;
            if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
                if k.kind == KeyEventKind::Press {
                    match self.mode {
                        Mode::Normal => self.on_normal_key(k.code),
                        Mode::Add | Mode::Edit(_) => self.on_input_key(k.code),
                    }
                }
            }
        }
        Ok(())
    }

    fn on_normal_key(&mut self, code: KeyCode) {
        self.status = None;
        let len = self.tasks.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.selected = move_selection(self.selected, len, 1),
            KeyCode::Up | KeyCode::Char('k') => self.selected = move_selection(self.selected, len, -1),
            KeyCode::Char('a') => {
                self.input.clear();
                self.mode = Mode::Add;
            }
            KeyCode::Char('e') => {
                if let Some(i) = self.selected_task() {
                    self.input = self.tasks[i].title.clone();
                    self.mode = Mode::Edit(self.tasks[i].id);
                }
            }
            KeyCode::Char('d') | KeyCode::Char(' ') => {
                if self.toggle_done() {
                    self.persist();
                }
            }
            KeyCode::Char('r') => {
                if self.revive_selected() {
                    self.persist();
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if self.remove_selected() {
                    self.persist();
                }
            }
            _ => {}
        }
    }

    fn on_input_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Enter => {
                let title = self.input.trim().to_string();
                if !title.is_empty() {
                    match self.mode {
                        Mode::Add => self.add_task(title),
                        Mode::Edit(id) => self.apply_edit(id, title),
                        Mode::Normal => {}
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
        let task = Task::new(Task::next_id(&self.tasks), title);
        self.tasks.push(task);
        self.selected = 0; // freshly Hot -> top of the order
    }

    fn apply_edit(&mut self, id: u64, title: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.title = title;
            t.touch();
        }
        self.selected = 0;
    }

    fn toggle_done(&mut self) -> bool {
        match self.selected_task() {
            Some(i) => {
                self.tasks[i].done = !self.tasks[i].done;
                true
            }
            None => false,
        }
    }

    fn revive_selected(&mut self) -> bool {
        match self.selected_task() {
            Some(i) => {
                self.tasks[i].touch();
                self.selected = 0;
                true
            }
            None => false,
        }
    }

    fn remove_selected(&mut self) -> bool {
        match self.selected_task() {
            Some(i) => {
                self.tasks.remove(i);
                self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
                true
            }
            None => false,
        }
    }

    fn persist(&mut self) {
        if let Err(e) = store::save(&self.cfg.storage_path, &self.tasks) {
            self.status = Some(format!("save failed: {e}"));
        }
    }

    fn draw(&self, f: &mut Frame) {
        let now = model::now();
        let th = self.cfg.thresholds();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        f.render_widget(
            Paragraph::new(Line::from("zym — fermenting todo".fg(NEON_ACCENT).bold())),
            header,
        );

        let order = self.order();
        let items: Vec<ListItem> = order
            .iter()
            .map(|&i| {
                let t = &self.tasks[i];
                let st = t.status(&th, now);
                let (done, total) = t.progress();
                let prog = if total > 0 { format!("  [{done}/{total}]") } else { String::new() };
                let mark = if t.done { "✓" } else { "·" };
                let mut style = status_style(st);
                if t.done {
                    style = style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
                }
                ListItem::new(Line::styled(format!("{mark} {}{prog}", t.title), style))
            })
            .collect();

        let mut state = ListState::default();
        if !order.is_empty() {
            state.select(Some(self.selected.min(order.len() - 1)));
        }

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" tasks ({}) ", order.len())),
            )
            .highlight_symbol("▶ ")
            .highlight_style(Style::default().fg(NEON_ACCENT).add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, body, &mut state);

        let footer_line = match self.mode {
            Mode::Add => Line::from(format!("add> {}", self.input).fg(NEON_HOT)),
            Mode::Edit(_) => Line::from(format!("edit> {}", self.input).fg(NEON_HOT)),
            Mode::Normal => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from("a add · e edit · d done · r revive · x del · q quit".dim()),
            },
        };
        f.render_widget(Paragraph::new(footer_line), footer);
    }
}

fn move_selection(cur: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as i32 + delta).clamp(0, len as i32 - 1) as usize
}

// Neon-ish palette. ponytail: flat per-status colors for now; the decay gradient
// ramp lands in the juice slice.
const NEON_ACCENT: Color = Color::Rgb(0, 255, 213); // teal header/selection
const NEON_HOT: Color = Color::Rgb(255, 60, 172); // magenta hot / input

fn status_style(s: Status) -> Style {
    match s {
        Status::Hot => Style::default().fg(NEON_HOT).add_modifier(Modifier::BOLD),
        Status::Decaying => Style::default().fg(Color::Rgb(150, 150, 170)),
        Status::Dormant => Style::default().fg(Color::Rgb(90, 90, 110)).add_modifier(Modifier::DIM),
        Status::Bubbling => Style::default().fg(NEON_ACCENT).add_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(n: usize) -> App {
        let tasks = (0..n).map(|i| Task::new(i as u64 + 1, format!("t{i}"))).collect();
        App::new(Config::default(), tasks)
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
    fn toggle_done_flips_selected() {
        let mut app = app_with(1);
        assert!(!app.tasks[0].done);
        assert!(app.toggle_done());
        assert!(app.tasks[0].done);
        assert!(app.toggle_done());
        assert!(!app.tasks[0].done);
    }

    #[test]
    fn remove_clamps_selection() {
        let mut app = app_with(3);
        app.selected = 2;
        assert!(app.remove_selected());
        assert_eq!(app.tasks.len(), 2);
        assert_eq!(app.selected, 1); // was 2, clamped to new last
    }

    #[test]
    fn mutations_on_empty_are_noops() {
        let mut app = app_with(0);
        assert!(!app.toggle_done());
        assert!(!app.remove_selected());
        assert!(!app.revive_selected());
    }
}
