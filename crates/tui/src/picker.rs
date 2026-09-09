use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Model,
    Reasoning,
    Session,
}

pub struct Item {
    pub label: String,
    pub value: String,
}

pub struct Picker {
    pub kind: Kind,
    items: Vec<Item>,
    query: String,
    selected: usize,
}

impl Picker {
    pub fn new(kind: Kind, items: Vec<Item>, current: &str) -> Self {
        let selected = items
            .iter()
            .position(|item| item.value == current)
            .unwrap_or_default();
        Self {
            kind,
            items,
            query: String::new(),
            selected,
        }
    }

    fn filtered(&self) -> Vec<&Item> {
        let query = self.query.to_lowercase();
        self.items
            .iter()
            .filter(|item| item.label.to_lowercase().contains(&query))
            .collect()
    }

    pub fn key(&mut self, key: KeyCode) -> Option<String> {
        match key {
            KeyCode::Char(c) => {
                self.query.push(c);
                self.selected = 0;
            }
            KeyCode::Backspace => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.selected = 0;
            }
            KeyCode::Up => {
                let count = self.filtered().len();
                if count > 0 {
                    self.selected = self.selected.checked_sub(1).unwrap_or(count - 1);
                }
            }
            KeyCode::Down => {
                let count = self.filtered().len();
                if count > 0 {
                    self.selected = (self.selected + 1) % count;
                }
            }
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(8),
            KeyCode::PageDown => {
                self.selected = self
                    .selected
                    .saturating_add(8)
                    .min(self.filtered().len().saturating_sub(1));
            }
            KeyCode::Enter => {
                return self
                    .filtered()
                    .get(self.selected)
                    .map(|item| item.value.clone());
            }
            _ => {}
        }
        None
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let height = area.height.saturating_sub(4).min(28);
        let width = area.width.saturating_sub(4).min(86);
        let [_, center, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .areas(area);
        let [_, center, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(width),
            Constraint::Fill(1),
        ])
        .areas(center);
        frame.render_widget(Clear, center);
        let title = match self.kind {
            Kind::Model => " MODEL · OpenRouter tool-capable catalog ",
            Kind::Reasoning => " REASONING · available for selected model ",
            Kind::Session => " SESSION · current working directory ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Rgb(103, 166, 255)));
        let inner = block.inner(center);
        frame.render_widget(block, center);
        let [search, list, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let filtered = self.filtered();
        let query = if self.query.is_empty() {
            "Type to filter…".to_owned()
        } else {
            format!("› {}", self.query)
        };
        frame.render_widget(Paragraph::new(query).fg(Color::Rgb(240, 190, 92)), search);
        let items = if filtered.is_empty() {
            vec![ListItem::new("  No matches")]
        } else {
            filtered
                .iter()
                .map(|item| ListItem::new(item.label.clone()))
                .collect::<Vec<_>>()
        };
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(Color::Rgb(49, 60, 78))
                        .fg(Color::White)
                        .bold(),
                )
                .highlight_symbol("› "),
            list,
            &mut state,
        );
        frame.render_widget(
            Paragraph::new(format!(
                "↑/↓ choose · PgUp/PgDn jump · Enter apply · Esc cancel    {} match{}",
                filtered.len(),
                if filtered.len() == 1 { "" } else { "es" }
            ))
            .fg(Color::DarkGray),
            help,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_case_insensitively_and_handles_empty_results() {
        let mut picker = Picker::new(
            Kind::Model,
            vec![
                Item {
                    label: "Alpha".into(),
                    value: "a".into(),
                },
                Item {
                    label: "Beta".into(),
                    value: "b".into(),
                },
            ],
            "a",
        );
        picker.key(KeyCode::Char('B'));
        assert_eq!(picker.key(KeyCode::Enter).as_deref(), Some("b"));
        picker.key(KeyCode::Char('z'));
        picker.key(KeyCode::Down);
        assert!(picker.key(KeyCode::Enter).is_none());
    }
}
