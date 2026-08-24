//! Interactive TUI. Slice 1: read-only task list, status styling, navigation.
//!
//! Terminal setup/teardown goes through `ratatui::init`/`restore`, which install
//! a panic hook that restores the terminal — so a crash never leaves the user's
//! shell in raw mode.

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

struct App {
    cfg: Config,
    tasks: Vec<Task>,
    selected: usize,
    quit: bool,
}

impl App {
    fn new(cfg: Config, tasks: Vec<Task>) -> Self {
        Self { cfg, tasks, selected: 0, quit: false }
    }

    /// Display order: most-recently-updated first.
    fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.tasks.len()).collect();
        idx.sort_by(|&a, &b| self.tasks[b].last_updated.cmp(&self.tasks[a].last_updated));
        idx
    }

    // Slice 1 is static, so block on input (no idle redraws). The capped tick
    // loop arrives with animation in slice 3.
    fn run(&mut self, term: &mut DefaultTerminal) -> Result<(), String> {
        while !self.quit {
            term.draw(|f| self.draw(f)).map_err(|e| e.to_string())?;
            if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
                if k.kind == KeyEventKind::Press {
                    self.on_key(k.code);
                }
            }
        }
        Ok(())
    }

    fn on_key(&mut self, code: KeyCode) {
        let len = self.tasks.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = move_selection(self.selected, len, 1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = move_selection(self.selected, len, -1)
            }
            _ => {}
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

        f.render_widget(Paragraph::new(Line::from("zym — fermenting todo").bold()), header);

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
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, body, &mut state);

        f.render_widget(Paragraph::new(Line::from("j/k move · q quit").dim()), footer);
    }
}

fn move_selection(cur: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as i32 + delta).clamp(0, len as i32 - 1) as usize
}

fn status_style(s: Status) -> Style {
    match s {
        Status::Hot => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Status::Decaying => Style::default().fg(Color::Gray),
        Status::Dormant => Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        Status::Bubbling => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_clamps_to_bounds() {
        assert_eq!(move_selection(0, 0, 1), 0); // empty list
        assert_eq!(move_selection(0, 3, -1), 0); // can't go below 0
        assert_eq!(move_selection(2, 3, 1), 2); // can't exceed last
        assert_eq!(move_selection(1, 3, 1), 2);
        assert_eq!(move_selection(1, 3, -1), 0);
    }
}
