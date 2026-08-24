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
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{self, Config};
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
    Add,
    Edit(u64),
    AddSub(u64),
}

/// A rendered line: either a task or one of its subtasks (indices into `tasks`).
#[derive(Clone, Copy)]
enum Row {
    Task(usize),
    Sub(usize, usize),
}

struct App {
    cfg: Config,
    tasks: Vec<Task>,
    selected: usize,
    mode: Mode,
    input: String,
    status: Option<String>,
    show_dormant: bool,
    expanded: HashSet<u64>,
    frame: u64,
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
            show_dormant: false,
            expanded: HashSet::new(),
            frame: 0,
            quit: false,
        }
    }

    /// Visible task indices: dormant hidden unless toggled, then ordered by
    /// lifecycle priority (Hot/Bubbling rise) and recency within a band.
    fn visible(&self) -> Vec<usize> {
        let now = model::now();
        let th = self.cfg.thresholds();
        let mut idx: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| self.show_dormant || self.tasks[i].status(&th, now) != Status::Dormant)
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
            Some(Row::Task(ti)) | Some(Row::Sub(ti, _)) => Some(ti),
            None => None,
        }
    }

    fn has_animation(&self) -> bool {
        let now = model::now();
        let th = self.cfg.thresholds();
        self.visible()
            .iter()
            .any(|&i| matches!(self.tasks[i].status(&th, now), Status::Hot | Status::Bubbling))
    }

    fn run(&mut self, term: &mut DefaultTerminal) -> Result<(), String> {
        let tick = Duration::from_millis((1000 / self.cfg.tick_fps.max(1)) as u64);
        let mut dirty = true;
        while !self.quit {
            if dirty {
                term.draw(|f| self.draw(f)).map_err(|e| e.to_string())?;
                dirty = false;
            }
            if event::poll(tick).map_err(|e| e.to_string())? {
                if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
                    if k.kind == KeyEventKind::Press {
                        match self.mode {
                            Mode::Normal => self.on_normal_key(k.code),
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
            KeyCode::Down | KeyCode::Char('j') => self.selected = move_selection(self.selected, rows, 1),
            KeyCode::Up | KeyCode::Char('k') => self.selected = move_selection(self.selected, rows, -1),
            KeyCode::Tab => {
                self.show_dormant = !self.show_dormant;
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
            KeyCode::Char('e') => {
                if let Some(i) = self.selected_task() {
                    self.input = self.tasks[i].title.clone();
                    self.mode = Mode::Edit(self.tasks[i].id);
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
                        Mode::AddSub(id) => self.add_subtask(id, title),
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
        self.tasks.push(Task::new(Task::next_id(&self.tasks), title));
        self.selected = 0;
    }

    fn apply_edit(&mut self, id: u64, title: String) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.title = title;
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
            None => false,
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

    fn revive_selected(&mut self) -> bool {
        match self.selected_task() {
            Some(ti) => {
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
            None => return false,
        }
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
        true
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

        let rows = self.rows();
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| match *row {
                Row::Task(ti) => {
                    let t = &self.tasks[ti];
                    let (mark, mut style) = row_style(t.status(&th, now), t.age(now), &th, self.frame);
                    let (done, total) = t.progress();
                    let expand = if total > 0 {
                        let caret = if self.expanded.contains(&t.id) { "▾" } else { "▸" };
                        format!(" {caret} [{done}/{total}]")
                    } else {
                        String::new()
                    };
                    let mark = if t.done { "✓" } else { mark };
                    if t.done {
                        style = style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
                    }
                    ListItem::new(Line::styled(format!("{mark} {}{expand}", t.title), style))
                }
                Row::Sub(ti, si) => {
                    let s = &self.tasks[ti].subtasks[si];
                    let mark = if s.done { "✓" } else { "·" };
                    let mut style = Style::default().fg(SUBTASK);
                    if s.done {
                        style = style.add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
                    }
                    ListItem::new(Line::styled(format!("    ↳ {mark} {}", s.title), style))
                }
            })
            .collect();

        let mut state = ListState::default();
        if !rows.is_empty() {
            state.select(Some(self.selected.min(rows.len() - 1)));
        }

        let count = self.visible().len();
        let title = if self.show_dormant {
            format!(" tasks ({count}) · dormant shown ")
        } else {
            format!(" tasks ({count}) ")
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_symbol("▶ ")
            .highlight_style(Style::default().fg(NEON_ACCENT).add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, body, &mut state);

        let footer_line = match self.mode {
            Mode::Add => Line::from(format!("add> {}", self.input).fg(NEON_HOT)),
            Mode::AddSub(_) => Line::from(format!("subtask> {}", self.input).fg(NEON_HOT)),
            Mode::Edit(_) => Line::from(format!("edit> {}", self.input).fg(NEON_HOT)),
            Mode::Normal => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from(
                    "a add · s +sub · e edit · enter expand · space done · x del · r revive · tab dormant · q"
                        .dim(),
                ),
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

fn sort_rank(s: Status) -> u8 {
    match s {
        Status::Hot => 0,
        Status::Bubbling => 1,
        Status::Decaying => 2,
        Status::Dormant => 3,
    }
}

// --- palette + animation (pure, testable) ---

const NEON_ACCENT: Color = Color::Rgb(0, 255, 213);
const NEON_HOT: Color = Color::Rgb(255, 60, 172);
const NEON_BUBBLE: Color = Color::Rgb(90, 230, 255);
const DECAY_FRESH: Color = Color::Rgb(150, 200, 210);
const DECAY_STALE: Color = Color::Rgb(95, 95, 120);
const DORMANT: Color = Color::Rgb(80, 80, 105);
const SUBTASK: Color = Color::Rgb(140, 150, 170);
const BUBBLES: [&str; 4] = ["·", "∘", "○", "°"];

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

fn decay_color(age: Duration, th: &Thresholds) -> Color {
    let start = th.hot_window.as_secs() as f32;
    let end = th.dormant_after.as_secs() as f32;
    let t = if end > start {
        (age.as_secs() as f32 - start) / (end - start)
    } else {
        0.0
    };
    lerp_rgb(DECAY_FRESH, DECAY_STALE, t)
}

fn row_style(st: Status, age: Duration, th: &Thresholds, frame: u64) -> (&'static str, Style) {
    match st {
        Status::Hot => ("·", Style::default().fg(breathe(NEON_HOT, frame)).add_modifier(Modifier::BOLD)),
        Status::Bubbling => (
            bubble_glyph(frame),
            Style::default().fg(breathe(NEON_BUBBLE, frame)).add_modifier(Modifier::BOLD),
        ),
        Status::Decaying => ("·", Style::default().fg(decay_color(age, th))),
        Status::Dormant => ("·", Style::default().fg(DORMANT).add_modifier(Modifier::DIM)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SubTask;

    fn app_with(n: usize) -> App {
        let tasks = (0..n).map(|i| Task::new(i as u64 + 1, format!("t{i}"))).collect();
        App::new(Config::default(), tasks)
    }

    fn th() -> Thresholds {
        Config::default().thresholds()
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
            SubTask { title: "a".into(), done: false },
            SubTask { title: "b".into(), done: false },
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
            SubTask { title: "a".into(), done: false },
            SubTask { title: "b".into(), done: false },
        ];
        app.toggle_expand(); // selected is task row 0
        app.selected = 1; // first subtask row
        assert!(app.remove_selected());
        assert_eq!(app.tasks.len(), 1); // task survives
        assert_eq!(app.tasks[0].subtasks.len(), 1);
        assert_eq!(app.tasks[0].subtasks[0].title, "b");
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
        assert_eq!(decay_color(t.hot_window, &t), DECAY_FRESH);
        assert_eq!(decay_color(t.dormant_after, &t), DECAY_STALE);
        let mid = decay_color((t.hot_window + t.dormant_after) / 2, &t);
        assert_ne!(mid, DECAY_FRESH);
        assert_ne!(mid, DECAY_STALE);
    }

    #[test]
    fn breathe_never_brighter_than_base() {
        for frame in 0..48u64 {
            let (r, g, b) = to_rgb(breathe(NEON_HOT, frame));
            let (br, bg, bb) = to_rgb(NEON_HOT);
            assert!(r <= br && g <= bg && b <= bb);
        }
    }

    #[test]
    fn dormant_hidden_until_toggled() {
        let mut app = app_with(0);
        app.tasks.push(Task::new(1, "fresh"));
        let mut mid = Task::new(2, "mid");
        mid.last_updated = model::now() - 16 * 86_400; // dormant band
        app.tasks.push(mid);
        let hidden = app.visible().len();
        app.show_dormant = true;
        assert!(app.visible().len() > hidden);
    }
}
