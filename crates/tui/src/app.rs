use crate::{
    Command, Outcome,
    picker::{Item, Kind, Picker},
};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use ri_agent::{
    agent::{Selection, Session},
    events::Event,
    openrouter::{Model, ReasoningEffort},
};
use std::collections::VecDeque;
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_LINES: usize = 4000;

pub(crate) struct App {
    selection: Selection,
    id: String,
    pub input: String,
    lines: VecDeque<Line<'static>>,
    status: String,
    busy: bool,
    scroll_back: u16,
    models: Vec<Model>,
    pub catalog_loading: bool,
    picker: Option<Picker>,
    persistence: bool,
}

impl App {
    pub fn new(session: &Session, interrupted: bool) -> Self {
        let mut app = Self {
            selection: session.selection(),
            id: session.id().to_owned(),
            input: String::new(),
            lines: VecDeque::new(),
            status: "Loading OpenRouter catalog…".into(),
            busy: false,
            scroll_back: 0,
            models: Vec::new(),
            catalog_loading: true,
            picker: None,
            persistence: session.store().is_some(),
        };
        app.append(
            "HARNESS",
            "Describe a task to begin. Tools run locally without approval.",
            Color::Yellow,
        );
        for event in session.history() {
            app.event(event);
        }
        if interrupted {
            app.interruption();
        }
        app
    }

    fn interruption(&mut self) {
        self.append("INTERRUPTED", "Restored the last completed prompt. No tools were replayed; local tool effects may remain.", Color::Yellow);
    }

    fn append(&mut self, role: &str, text: &str, color: Color) {
        self.lines
            .push_back(Line::from(role.to_owned()).fg(color).bold());
        for line in text.lines() {
            self.lines.push_back(Line::from(
                line.chars().filter(|c| !c.is_control()).collect::<String>(),
            ));
            while self.lines.len() > MAX_LINES {
                self.lines.pop_front();
            }
        }
        self.lines.push_back(Line::default());
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }

    pub fn event(&mut self, event: Event) {
        match event {
            Event::User(text) => self.append("YOU", &text, Color::Cyan),
            Event::Assistant(text) => self.append("ASSISTANT", &text, Color::Green),
            Event::Tool(text) => self.append("TOOL", &text, Color::Blue),
            Event::Progress(text) => {
                self.status.clone_from(&text);
                self.append("ACTIVITY", &text, Color::DarkGray);
            }
        }
    }

    pub fn finish(&mut self, result: Result<Outcome, String>) {
        self.busy = false;
        self.status = "Ready".into();
        match result {
            Ok(Outcome::Updated { id, selection }) => {
                self.id = id;
                self.selection = selection;
            }
            Ok(Outcome::Loaded {
                id,
                selection,
                history,
                interrupted,
            }) => {
                self.id = id;
                self.selection = selection;
                self.lines.clear();
                self.scroll_back = 0;
                for event in history {
                    self.event(event);
                }
                if interrupted {
                    self.interruption();
                }
                self.status = "Session restored".into();
            }
            Ok(Outcome::Sessions(sessions)) => {
                let items = sessions
                    .into_iter()
                    .map(|session| Item {
                        label: format!(
                            "{} · {} · {}{}",
                            session.updated_at,
                            session.title,
                            session.model,
                            if session.interrupted {
                                " · interrupted"
                            } else {
                                ""
                            }
                        ),
                        value: session.id,
                    })
                    .collect();
                self.picker = Some(Picker::new(Kind::Session, items, &self.id));
            }
            Err(error) => {
                self.status = "Failed · see transcript".into();
                self.append("ERROR", &error, Color::Red);
            }
        }
    }

    pub fn catalog(&mut self, result: Result<Vec<Model>, String>) {
        self.catalog_loading = false;
        match result {
            Ok(models) => {
                self.models = models;
                self.status = "Ready · F2 model · F3 reasoning · F4 sessions".into();
            }
            Err(error) => self.append(
                "CATALOG",
                &format!("{error}\nF5 retries. The configured model remains usable."),
                Color::Yellow,
            ),
        }
    }

    fn available_efforts(&self) -> Vec<ReasoningEffort> {
        self.models
            .iter()
            .find(|model| model.id == self.selection.model)
            .map(Model::efforts)
            .unwrap_or_default()
    }

    fn send(&mut self, commands: &mpsc::UnboundedSender<Command>, command: Command) -> Result<()> {
        commands.send(command).context("Agent worker stopped")?;
        self.busy = true;
        Ok(())
    }

    pub fn submit(&mut self, commands: &mpsc::UnboundedSender<Command>) -> Result<()> {
        if self.busy || self.catalog_loading || self.input.trim().is_empty() {
            return Ok(());
        }
        if let Some(effort) = self.selection.reasoning_effort
            && !self.available_efforts().contains(&effort)
        {
            self.append("REASONING", "The catalog does not advertise this effort. Use F3 to choose Provider default, or F5 to refresh before sending.", Color::Yellow);
            return Ok(());
        }
        let prompt = std::mem::take(&mut self.input);
        self.append("YOU", &prompt, Color::Cyan);
        self.scroll_back = 0;
        self.status = "Working".into();
        self.send(commands, Command::Submit(prompt))
    }

    fn open_picker(&mut self, kind: Kind) {
        let (items, current) = match kind {
            Kind::Model => (
                self.models
                    .iter()
                    .map(|model| Item {
                        label: format!("{} · {}", model.id, model.name),
                        value: model.id.clone(),
                    })
                    .collect(),
                self.selection.model.clone(),
            ),
            Kind::Reasoning => {
                let mut items = vec![Item {
                    label: "Provider default (omit reasoning override)".into(),
                    value: "default".into(),
                }];
                items.extend(self.available_efforts().into_iter().map(|effort| Item {
                    label: effort.to_string(),
                    value: effort.to_string(),
                }));
                (
                    items,
                    self.selection
                        .reasoning_effort
                        .map_or_else(|| "default".into(), |effort| effort.to_string()),
                )
            }
            Kind::Session => return,
        };
        self.picker = Some(Picker::new(kind, items, &current));
    }

    pub fn key(
        &mut self,
        key: KeyEvent,
        commands: &mpsc::UnboundedSender<Command>,
    ) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        if let Some(picker) = &mut self.picker {
            if key.code == KeyCode::Esc {
                self.picker = None;
                return Ok(false);
            }
            let kind = picker.kind;
            if let Some(value) = picker.key(key.code) {
                self.picker = None;
                let command = match kind {
                    Kind::Model => Command::Select(Selection {
                        model: value,
                        reasoning_effort: None,
                    }),
                    Kind::Reasoning => Command::Select(Selection {
                        model: self.selection.model.clone(),
                        reasoning_effort: match value.as_str() {
                            "default" => None,
                            _ => Some(value.parse()?),
                        },
                    }),
                    Kind::Session => Command::Resume(value),
                };
                self.send(commands, command)?;
            }
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('l') && !self.busy {
                self.send(commands, Command::Clear)?;
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::F(2) if !self.busy => self.open_picker(Kind::Model),
            KeyCode::F(3) if !self.busy => self.open_picker(Kind::Reasoning),
            KeyCode::F(4) if !self.busy && self.persistence => {
                self.send(commands, Command::ListSessions)?
            }
            KeyCode::Enter => self.submit(commands)?,
            KeyCode::Backspace => {
                if let Some((index, _)) = self.input.grapheme_indices(true).next_back() {
                    self.input.truncate(index);
                }
            }
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            _ => {}
        }
        Ok(false)
    }

    pub fn paste(&mut self, text: &str) {
        if self.picker.is_some() {
            return;
        }
        self.input.extend(
            text.chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .filter(|c| !c.is_control()),
        );
    }

    pub fn draw(&self, frame: &mut Frame) {
        let [header, selection, transcript, status, input, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(
            Paragraph::new(format!(
                " ri-agent / OpenRouter / {}{}",
                self.id,
                if self.persistence {
                    ""
                } else {
                    " · not saved"
                }
            ))
            .fg(Color::DarkGray),
            header,
        );
        frame.render_widget(
            Paragraph::new(format!(
                " {} · reasoning: {}",
                self.selection.model,
                self.selection
                    .reasoning_effort
                    .map_or_else(|| "provider default".into(), |effort| effort.to_string())
            ))
            .fg(Color::Cyan)
            .bold(),
            selection,
        );
        let width = usize::from(transcript.width.max(1));
        let mut lines = Vec::new();
        for line in &self.lines {
            let text = line.to_string();
            if text.is_empty() {
                lines.push(Line::default());
                continue;
            }
            let mut part = String::new();
            let mut count = 0;
            for grapheme in text.graphemes(true) {
                let cells = grapheme.width();
                if count + cells > width && !part.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut part)).style(line.style));
                    count = 0;
                }
                part.push_str(grapheme);
                count += cells;
            }
            if !part.is_empty() {
                lines.push(Line::from(part).style(line.style));
            }
        }
        let start = lines
            .len()
            .saturating_sub(usize::from(transcript.height))
            .saturating_sub(usize::from(self.scroll_back));
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(start)
                    .take(usize::from(transcript.height))
                    .collect::<Vec<_>>(),
            ),
            transcript,
        );
        frame.render_widget(
            Paragraph::new(self.status.as_str()).fg(Color::Yellow),
            status,
        );
        let title = if self.busy {
            " Draft · agent working "
        } else {
            " Prompt "
        };
        let available = usize::from(input.width.saturating_sub(3));
        let mut visible = self.input.as_str();
        while visible.width() > available {
            let Some(first) = visible.graphemes(true).next() else {
                break;
            };
            visible = visible.get(first.len()..).unwrap_or_default();
        }
        frame.render_widget(
            Paragraph::new(visible).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            input,
        );
        if input.width > 2 && input.height > 2 && self.picker.is_none() {
            frame.set_cursor_position((
                input.x + 1 + u16::try_from(visible.width()).unwrap_or_default(),
                input.y + 1,
            ));
        }
        frame.render_widget(
            Paragraph::new(
                "F2 model · F3 reasoning · F4 sessions · F5 refresh · Ctrl+L new · Ctrl+C quit",
            )
            .fg(Color::DarkGray),
            help,
        );
        if let Some(picker) = &self.picker {
            picker.draw(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use ri_agent::{agent::AgentConfig, config::LuaConfig, events::Output, limits::Limits};

    fn app() -> Result<App> {
        let session = Session::new(
            "http://localhost".into(),
            "mock".into(),
            AgentConfig {
                model: "mock".into(),
                reasoning_effort: None,
                max_turns: std::num::NonZeroUsize::MIN,
                limits: Limits::default(),
                lua: LuaConfig::from_source("return {}", "test")?,
            },
            Output::default(),
        );
        let mut app = App::new(&session, false);
        app.catalog_loading = false;
        Ok(app)
    }

    #[test]
    fn renders_small_terminals_and_bounds_history() -> Result<()> {
        let mut app = app()?;
        app.append("TOOL", &"line\n".repeat(MAX_LINES + 10), Color::Blue);
        assert_eq!(app.lines.len(), MAX_LINES);
        for (width, height) in [(80, 24), (10, 4), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            terminal.draw(|frame| app.draw(frame))?;
            app.open_picker(Kind::Reasoning);
            terminal.draw(|frame| app.draw(frame))?;
        }
        Ok(())
    }

    #[test]
    fn ignores_busy_submissions_and_offers_only_supported_efforts() -> Result<()> {
        let mut app = app()?;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.submit(&sender)?;
        assert!(receiver.try_recv().is_err());
        app.input = "hello".into();
        app.submit(&sender)?;
        assert!(matches!(receiver.try_recv()?, Command::Submit(prompt) if prompt == "hello"));
        app.input = "draft".into();
        app.submit(&sender)?;
        assert_eq!(app.input, "draft");
        assert!(receiver.try_recv().is_err());
        app.models = vec![serde_json::from_str(
            r#"{"id":"mock","reasoning":{"mandatory":true,"supported_efforts":["high","none"]}}"#,
        )?];
        assert_eq!(app.available_efforts(), vec![ReasoningEffort::High]);
        app.busy = false;
        app.selection.reasoning_effort = Some(ReasoningEffort::Low);
        app.submit(&sender)?;
        assert!(receiver.try_recv().is_err());
        Ok(())
    }
}
