use crate::{
    Command, Outcome,
    commands::{self, SlashCommand},
    picker::{Item, Kind, Picker},
};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use ri_agent::{
    FileMatches,
    agent::{Selection, Session},
    events::Event,
    find_files,
    openrouter::{Model, ReasoningEffort},
};
use std::collections::VecDeque;
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_LINES: usize = 4000;
const INK: Color = Color::Rgb(196, 205, 219);
const MUTED: Color = Color::Rgb(103, 116, 137);
const ACCENT: Color = Color::Rgb(103, 166, 255);
const SUCCESS: Color = Color::Rgb(102, 204, 170);
const WARNING: Color = Color::Rgb(240, 190, 92);
const PANEL: Color = Color::Rgb(49, 60, 78);

type FileSearch = std::pin::Pin<Box<dyn std::future::Future<Output = Result<FileMatches>> + Send>>;

pub struct App {
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
    command_selection: usize,
    file_search: Option<FileSearch>,
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
            command_selection: 0,
            file_search: None,
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
            Event::User(text) => self.append("YOU", &text, ACCENT),
            Event::Assistant(text) => self.append("ASSISTANT", &text, SUCCESS),
            Event::Tool(text) => self.append("TOOL", &text, Color::Rgb(177, 145, 255)),
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
            Ok(Outcome::Notice(text)) => self.append("LOGIN", &text, Color::Yellow),
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

    pub fn enter(&mut self, commands: &mpsc::UnboundedSender<Command>) -> Result<()> {
        if self.busy {
            return Ok(());
        }
        if self.input.trim_start().starts_with('/') {
            let input = std::mem::take(&mut self.input);
            if let Err(error) = self.slash_command(&input, commands) {
                self.append("COMMAND", &format!("{error:#}"), Color::Yellow);
            }
            return Ok(());
        }
        self.submit(commands)
    }

    fn slash_command(
        &mut self,
        input: &str,
        commands: &mpsc::UnboundedSender<Command>,
    ) -> Result<()> {
        match commands::parse(input)? {
            SlashCommand::Model(Some(query)) => {
                let model = self
                    .models
                    .iter()
                    .find(|model| model.id == query || model.id.contains(&query))
                    .map(|model| model.id.clone());
                let Some(model) = model else {
                    self.append(
                        "MODEL",
                        "No matching OpenRouter model; use /model to search.",
                        Color::Yellow,
                    );
                    return Ok(());
                };
                self.send(
                    commands,
                    Command::Select(Selection {
                        model,
                        reasoning_effort: None,
                    }),
                )?;
            }
            SlashCommand::Model(None) => self.open_picker(Kind::Model),
            SlashCommand::Reasoning(Some(effort)) => {
                if !self.available_efforts().contains(&effort) {
                    self.append("REASONING", "That effort is not advertised for the selected model. Use /reasoning to inspect supported levels.", Color::Yellow);
                    return Ok(());
                }
                self.send(
                    commands,
                    Command::Select(Selection {
                        model: self.selection.model.clone(),
                        reasoning_effort: Some(effort),
                    }),
                )?;
            }
            SlashCommand::Reasoning(None) => self.open_picker(Kind::Reasoning),
            SlashCommand::Resume(Some(id)) => self.send(commands, Command::Resume(id))?,
            SlashCommand::Resume(None) => {
                if self.persistence {
                    self.send(commands, Command::ListSessions)?;
                } else {
                    self.append(
                        "SESSION",
                        "Persistence is disabled; restart without --no-save to resume sessions.",
                        Color::Yellow,
                    );
                }
            }
            SlashCommand::Login => self.send(commands, Command::Login)?,
            SlashCommand::Help => self.append(
                "COMMANDS",
                "/model [query] · /reasoning [effort] · /resume [id] · /login · /help",
                Color::Cyan,
            ),
        }
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
            Kind::Session | Kind::File => return,
        };
        self.picker = Some(Picker::new(kind, items, &current));
    }

    pub async fn wait_files(&mut self) -> Result<FileMatches> {
        let result = match &mut self.file_search {
            Some(search) => search.await,
            None => std::future::pending().await,
        };
        self.file_search = None;
        result
    }

    pub fn files_ready(&mut self, result: Result<FileMatches>) {
        self.file_search = None;
        match result {
            Ok(files) => {
                self.status = if files.truncated {
                    "File list truncated; use Find to search a narrower directory".into()
                } else {
                    format!("{} files found", files.paths.len())
                };
                if let Some(picker) = &mut self.picker {
                    picker.set_items(
                        files
                            .paths
                            .into_iter()
                            .map(|path| Item {
                                label: path.escape_debug().to_string(),
                                value: path,
                            })
                            .collect(),
                    );
                }
            }
            Err(error) => {
                self.picker = None;
                self.append("ERROR", &format!("{error:#}"), Color::Red);
            }
        }
    }

    pub fn key(
        &mut self,
        key: KeyEvent,
        commands: &mpsc::UnboundedSender<Command>,
    ) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Ok(true);
        }
        if let Some(picker) = &mut self.picker {
            if key.code == KeyCode::Esc {
                self.picker = None;
                self.file_search = None;
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
                    Kind::File => {
                        self.file_search = None;
                        // JSON quoting preserves spaces and control characters in paths.
                        self.input.push('@');
                        self.input.push_str(&serde_json::to_string(&value)?);
                        self.input.push(' ');
                        return Ok(false);
                    }
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
        if self.command_completion_key(key.code, commands)? {
            return Ok(false);
        }
        match key.code {
            KeyCode::F(2) if !self.busy => self.open_picker(Kind::Model),
            KeyCode::F(3) if !self.busy => self.open_picker(Kind::Reasoning),
            KeyCode::F(4) if !self.busy && self.persistence => {
                self.send(commands, Command::ListSessions)?;
            }
            KeyCode::Enter => self.enter(commands)?,
            KeyCode::Backspace => {
                if let Some((index, _)) = self.input.grapheme_indices(true).next_back() {
                    self.input.truncate(index);
                    self.command_selection = 0;
                }
            }
            KeyCode::Char('@')
                if self.input.is_empty() || self.input.ends_with(char::is_whitespace) =>
            {
                self.picker = Some(Picker::new(Kind::File, Vec::new(), ""));
                self.status = "Finding files with fd…".into();
                self.file_search = Some(Box::pin(async {
                    find_files(
                        "",
                        ".",
                        ri_agent::limits::Limits {
                            command_timeout: std::time::Duration::from_secs(10),
                            max_output_bytes: std::num::NonZeroUsize::MIN
                                .saturating_add(1024 * 1024 - 1),
                        },
                    )
                    .await
                }));
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.command_selection = 0;
            }
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            _ => {}
        }
        Ok(false)
    }

    fn command_completion_key(
        &mut self,
        key: KeyCode,
        sender: &mpsc::UnboundedSender<Command>,
    ) -> Result<bool> {
        if !self.input.trim_start().starts_with('/') || self.input.chars().any(char::is_whitespace)
        {
            return Ok(false);
        }
        let suggestions = commands::suggestions(self.input.trim_start());
        if suggestions.is_empty() {
            return Ok(false);
        }
        self.command_selection = self
            .command_selection
            .min(suggestions.len().saturating_sub(1));
        match key {
            KeyCode::Up => {
                self.command_selection = self
                    .command_selection
                    .checked_sub(1)
                    .unwrap_or(suggestions.len() - 1);
            }
            KeyCode::Down => {
                self.command_selection = (self.command_selection + 1) % suggestions.len();
            }
            KeyCode::Tab => {
                if let Some(completion) =
                    commands::complete(self.input.trim_start(), self.command_selection)
                {
                    self.input = completion;
                    self.command_selection = 0;
                }
            }
            KeyCode::Enter => {
                let selected = suggestions
                    .get(self.command_selection)
                    .copied()
                    .unwrap_or_default();
                if self.input.trim() == selected {
                    self.enter(sender)?;
                } else if let Some(completion) =
                    commands::complete(self.input.trim_start(), self.command_selection)
                {
                    self.input = completion;
                    self.command_selection = 0;
                }
            }
            KeyCode::Esc => {
                self.input.clear();
                self.command_selection = 0;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(picker) = &mut self.picker {
            if picker.kind == Kind::File {
                for c in text.chars().filter(|c| !c.is_control()) {
                    picker.key(KeyCode::Char(c));
                }
            }
            return;
        }
        self.input.extend(
            text.chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .filter(|c| !c.is_control()),
        );
    }

    fn wrapped_transcript(&self, width: usize) -> Vec<Line<'static>> {
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
        lines
    }

    pub fn draw(&self, frame: &mut Frame) {
        let [rail, transcript, composer, help] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_rail(frame, rail);
        self.draw_transcript(frame, transcript);
        self.draw_composer(frame, composer);
        let hint = if help.width < 72 {
            " / commands  ·  F2 model  ·  F3 reasoning  ·  ^D quit"
        } else {
            " / commands  ·  F2 model  ·  F3 reasoning  ·  F4 sessions  ·  F5 refresh  ·  ^L new  ·  ^D quit"
        };
        frame.render_widget(Paragraph::new(hint).fg(MUTED), help);
        if let Some(picker) = &self.picker {
            picker.draw(frame);
        } else {
            self.draw_command_completion(frame, composer);
        }
    }

    fn draw_rail(&self, frame: &mut Frame, area: Rect) {
        let state = if self.busy {
            "● WORKING"
        } else if self.catalog_loading {
            "◌ SYNCING"
        } else {
            "● READY"
        };
        let state_color = if self.busy || self.catalog_loading {
            WARNING
        } else {
            SUCCESS
        };
        let persistence = if self.persistence {
            "SAVED"
        } else {
            "EPHEMERAL"
        };
        let reasoning = self
            .selection
            .reasoning_effort
            .map_or_else(|| "provider default".into(), |effort| effort.to_string());
        let lines = vec![
            Line::from(vec![
                Span::styled(state, Style::default().fg(state_color).bold()),
                Span::styled(
                    format!(
                        "  OpenRouter  ·  {persistence}  ·  session {}",
                        short_id(&self.id)
                    ),
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::from(vec![
                Span::styled("MODEL  ", Style::default().fg(MUTED)),
                Span::styled(&self.selection.model, Style::default().fg(ACCENT).bold()),
                Span::styled("    REASONING  ", Style::default().fg(MUTED)),
                Span::styled(reasoning, Style::default().fg(INK)),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" RI · AGENT WORKBENCH ")
                    .border_style(Style::default().fg(PANEL)),
            ),
            area,
        );
    }

    fn draw_transcript(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" TRANSCRIPT ")
            .border_style(Style::default().fg(PANEL));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.wrapped_transcript(usize::from(inner.width.max(1)));
        let start = lines
            .len()
            .saturating_sub(usize::from(inner.height))
            .saturating_sub(usize::from(self.scroll_back));
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(start)
                    .take(usize::from(inner.height))
                    .collect::<Vec<_>>(),
            )
            .fg(INK),
            inner,
        );
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect) {
        let title = if self.busy {
            " DRAFT · agent working "
        } else {
            " PROMPT · Enter sends · @ files "
        };
        let available = usize::from(area.width.saturating_sub(3));
        let mut visible = self.input.as_str();
        while visible.width() > available {
            let Some(first) = visible.graphemes(true).next() else {
                break;
            };
            visible = visible.get(first.len()..).unwrap_or_default();
        }
        let border = if self.busy { WARNING } else { ACCENT };
        frame.render_widget(
            Paragraph::new(visible).fg(INK).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_bottom(format!(" {} ", self.status))
                    .border_style(Style::default().fg(border)),
            ),
            area,
        );
        if area.width > 2 && area.height > 2 && self.picker.is_none() {
            frame.set_cursor_position((
                area.x + 1 + u16::try_from(visible.width()).unwrap_or_default(),
                area.y + 1,
            ));
        }
    }

    fn draw_command_completion(&self, frame: &mut Frame, input: Rect) {
        if !self.input.trim_start().starts_with('/') || self.input.chars().any(char::is_whitespace)
        {
            return;
        }
        let suggestions = commands::suggestions(self.input.trim_start());
        if suggestions.is_empty() {
            return;
        }
        let height = u16::try_from(suggestions.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2);
        let popup = Rect {
            x: input.x,
            y: input.y.saturating_sub(height),
            width: input.width.min(32),
            height,
        };
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Commands · ↑/↓ choose · Tab complete ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let items = suggestions
            .into_iter()
            .map(ListItem::new)
            .collect::<Vec<_>>();
        let selected = self.command_selection.min(items.len().saturating_sub(1));
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().bg(Color::DarkGray).bold()),
            inner,
            &mut state,
        );
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use ri_agent::{agent::AgentConfig, config::LuaConfig, events::Output, limits::Limits};

    fn app() -> Result<App> {
        let session = Session::new(
            std::sync::Arc::new(ri_agent::openrouter::OpenRouter::new(
                "http://localhost",
                ri_agent::credentials::api_key("mock")?,
            )?),
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
    fn selects_file_references_without_submitting_and_can_cancel() -> Result<()> {
        let mut app = app()?;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.input = "Review ".into();
        app.key(
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE),
            &sender,
        )?;
        assert!(app.file_search.is_some());
        app.paste("space");
        app.files_ready(Ok(FileMatches {
            paths: vec!["src/other.rs".into(), "src/space name.rs".into()],
            truncated: false,
        }));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &sender)?;
        assert_eq!(app.input, "Review @\"src/space name.rs\" ");
        assert!(app.picker.is_none());
        assert!(app.file_search.is_none());
        assert!(receiver.try_recv().is_err());
        app.key(
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE),
            &sender,
        )?;
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &sender)?;
        assert!(app.file_search.is_none());
        assert_eq!(app.input, "Review @\"src/space name.rs\" ");
        app.input = "user".into();
        app.key(
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE),
            &sender,
        )?;
        assert_eq!(app.input, "user@");
        assert!(app.picker.is_none());
        Ok(())
    }

    #[test]
    fn file_picker_errors_leave_prompt_intact() -> Result<()> {
        let mut app = app()?;
        let (sender, _) = mpsc::unbounded_channel();
        app.input = "Review ".into();
        app.key(
            KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE),
            &sender,
        )?;
        app.files_ready(Err(anyhow::anyhow!("fd unavailable")));
        assert!(app.picker.is_none());
        assert_eq!(app.input, "Review ");
        assert!(
            app.lines
                .iter()
                .any(|line| line.to_string().contains("fd unavailable"))
        );
        Ok(())
    }

    #[test]
    fn supports_ctrl_d_and_slash_commands() -> Result<()> {
        let mut app = app()?;
        let (sender, _receiver) = mpsc::unbounded_channel();
        assert!(app.key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &sender
        )?);
        app.input = "/".into();
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &sender)?;
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &sender)?;
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &sender)?;
        assert_eq!(app.input, "/resume ");
        app.input = "/help".into();
        app.enter(&sender)?;
        assert!(
            app.lines
                .iter()
                .any(|line| line.to_string().contains("/model"))
        );
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
