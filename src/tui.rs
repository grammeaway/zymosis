//! Interactive TUI.
//! Slice 1: read-only list, status styling, navigation.
//! Slice 2: add/edit/done/revive/delete with atomic save-on-change.
//! Slice 3: capped tick loop + juice (bubbling animation, hot breathe, decay
//!          color ramp) and a dormant-section toggle.
//!
//! Terminal setup/teardown goes through `ratatui::try_init`/`try_restore`, which
//! install a panic hook that restores the terminal.

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::config::{self, Config};
use crate::model::{self, Status, Task, Thresholds};
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
}

struct App {
    cfg: Config,
    tasks: Vec<Task>,
    selected: usize,
    mode: Mode,
    input: String,
    status: Option<String>,
    show_dormant: bool,
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
            frame: 0,
            quit: false,
        }
    }

    /// Visible rows: dormant hidden unless toggled, then ordered by lifecycle
    /// priority (Hot and Bubbling rise to the top) and recency within a band.
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

    fn selected_task(&self) -> Option<usize> {
        self.visible().get(self.selected).copied()
    }

    /// Only Hot/Bubbling rows animate, so a list without them needs no redraw.
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
                            Mode::Add | Mode::Edit(_) => self.on_input_key(k.code),
                        }
                        dirty = true;
                    }
                }
            } else {
                // tick: advance animation, redraw only if something moves.
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
        let len = self.tasks.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.selected = move_selection(self.selected, len, 1),
            KeyCode::Up | KeyCode::Char('k') => self.selected = move_selection(self.selected, len, -1),
            KeyCode::Tab => {
                self.show_dormant = !self.show_dormant;
                self.selected = 0;
            }
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

        let order = self.visible();
        let items: Vec<ListItem> = order
            .iter()
            .map(|&i| {
                let t = &self.tasks[i];
                let (mark, mut style) = row_style(t.status(&th, now), t.age(now), &th, self.frame);
                let (done, total) = t.progress();
                let prog = if total > 0 { format!("  [{done}/{total}]") } else { String::new() };
                let mark = if t.done { "✓" } else { mark };
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

        let title = if self.show_dormant {
            format!(" tasks ({}) · dormant shown ", order.len())
        } else {
            format!(" tasks ({}) ", order.len())
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_symbol("▶ ")
            .highlight_style(Style::default().fg(NEON_ACCENT).add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, body, &mut state);

        let footer_line = match self.mode {
            Mode::Add => Line::from(format!("add> {}", self.input).fg(NEON_HOT)),
            Mode::Edit(_) => Line::from(format!("edit> {}", self.input).fg(NEON_HOT)),
            Mode::Normal => match &self.status {
                Some(msg) => Line::from(msg.clone().fg(Color::Red)),
                None => Line::from(
                    "a add · e edit · d done · r revive · x del · tab dormant · q quit".dim(),
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

/// Lifecycle priority for list ordering: Hot and Bubbling rise, Dormant sinks.
fn sort_rank(s: Status) -> u8 {
    match s {
        Status::Hot => 0,
        Status::Bubbling => 1,
        Status::Decaying => 2,
        Status::Dormant => 3,
    }
}

// --- palette + animation (pure, testable) ---

const NEON_ACCENT: Color = Color::Rgb(0, 255, 213); // teal
const NEON_HOT: Color = Color::Rgb(255, 60, 172); // magenta
const NEON_BUBBLE: Color = Color::Rgb(90, 230, 255); // cyan
const DECAY_FRESH: Color = Color::Rgb(150, 200, 210);
const DECAY_STALE: Color = Color::Rgb(95, 95, 120);
const DORMANT: Color = Color::Rgb(80, 80, 105);
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

/// Gentle brightness pulse (triangle wave, 0.75..1.0) for hot/bubbling rows.
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

/// Cycling rising-bubble glyph for bubbling rows.
fn bubble_glyph(frame: u64) -> &'static str {
    BUBBLES[((frame / 2) % BUBBLES.len() as u64) as usize]
}

/// Fade a decaying row's colour from fresh toward stale across its band.
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

/// (mark glyph, base style) for a row given its lifecycle state.
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
    fn toggle_done_flips_selected() {
        let mut app = app_with(1);
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
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn mutations_on_empty_are_noops() {
        let mut app = app_with(0);
        assert!(!app.toggle_done());
        assert!(!app.remove_selected());
        assert!(!app.revive_selected());
    }

    #[test]
    fn sort_priority_hot_bubbling_first() {
        assert!(sort_rank(Status::Hot) < sort_rank(Status::Bubbling));
        assert!(sort_rank(Status::Bubbling) < sort_rank(Status::Decaying));
        assert!(sort_rank(Status::Decaying) < sort_rank(Status::Dormant));
    }

    #[test]
    fn bubble_glyph_cycles() {
        let seq: Vec<&str> = (0..8).map(bubble_glyph).collect();
        assert_eq!(seq[0], "·");
        assert_eq!(bubble_glyph(0), bubble_glyph(2 * BUBBLES.len() as u64 * 1)); // period repeats
    }

    #[test]
    fn decay_color_ramps_between_endpoints() {
        let t = th();
        assert_eq!(decay_color(t.hot_window, &t), DECAY_FRESH); // start of band
        assert_eq!(decay_color(t.dormant_after, &t), DECAY_STALE); // end of band
        // midpoint sits strictly between the endpoints
        let mid = decay_color((t.hot_window + t.dormant_after) / 2, &t);
        assert_ne!(mid, DECAY_FRESH);
        assert_ne!(mid, DECAY_STALE);
    }

    #[test]
    fn breathe_stays_in_range_and_is_rgb() {
        for frame in 0..48u64 {
            let (r, g, b) = to_rgb(breathe(NEON_HOT, frame));
            let (br, bg, bb) = to_rgb(NEON_HOT);
            assert!(r <= br && g <= bg && b <= bb); // never brighter than base
        }
    }

    #[test]
    fn dormant_hidden_until_toggled() {
        let mut app = app_with(0);
        let mut stale = Task::new(1, "old");
        stale.last_updated = 0; // ancient -> dormant/bubbling
        app.tasks.push(stale);
        app.tasks.push(Task::new(2, "fresh")); // hot
        // default hides very old dormant rows... but very old becomes Bubbling,
        // so assert the toggle changes visibility count for a mid-dormant task.
        let mut mid = Task::new(3, "mid");
        mid.last_updated = model::now() - 16 * 86_400; // > dormant_after(14d), < +bubble
        app.tasks.push(mid);
        let hidden = app.visible().len();
        app.show_dormant = true;
        assert!(app.visible().len() > hidden);
    }
}
