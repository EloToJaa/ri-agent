use anyhow::{Context, Result, bail};
use crossterm::event::{Event as TerminalEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use ri_agent::{
    agent::{AgentConfig, Session},
    events::{Event, Output},
};
use std::{
    collections::VecDeque,
    io::{self, IsTerminal},
};
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_LINES: usize = 4000;

enum Command {
    Submit(String),
    Clear,
}

struct App {
    model: String,
    input: String,
    lines: VecDeque<Line<'static>>,
    status: String,
    busy: bool,
    scroll_back: u16,
}

impl App {
    fn new(model: String) -> Self {
        let mut app = Self {
            model,
            input: String::new(),
            lines: VecDeque::new(),
            status: "Ready".into(),
            busy: false,
            scroll_back: 0,
        };
        app.append(
            "HARNESS",
            "Describe a task to begin. Tools run locally without approval.",
            Color::Yellow,
        );
        app
    }

    fn append(&mut self, role: &str, text: &str, color: Color) {
        self.lines
            .push_back(Line::from(role.to_owned()).fg(color).bold());
        // Keep terminal control characters out of provider/tool output.
        for line in text.lines() {
            self.lines.push_back(Line::from(
                line.chars()
                    .filter(|c| !c.is_control() || *c == '\t')
                    .collect::<String>(),
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

    fn submit(&mut self, commands: &mpsc::UnboundedSender<Command>) -> Result<()> {
        if self.busy || self.input.trim().is_empty() {
            return Ok(());
        }
        let prompt = std::mem::take(&mut self.input);
        self.append("YOU", &prompt, Color::Cyan);
        self.scroll_back = 0;
        self.busy = true;
        self.status = "Working".into();
        commands
            .send(Command::Submit(prompt))
            .context("Agent worker stopped")
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Progress(text) => {
                self.status = text.clone();
                self.append("ACTIVITY", &text, Color::DarkGray);
            }
            Event::Assistant(text) => self.append("ASSISTANT", &text, Color::Green),
            Event::Tool(text) => self.append("TOOL", &text, Color::Blue),
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let [header, transcript, status, input, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(
            Paragraph::new(format!(" ri-agent / {}", self.model))
                .style(Style::default().fg(Color::Cyan).bold()),
            header,
        );
        // Physical lines keep scrolling predictable even for very large tool results.
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
            visible = &visible[first.len()..];
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
        if input.width > 2 && input.height > 2 {
            frame.set_cursor_position((input.x + 1 + visible.width() as u16, input.y + 1));
        }
        frame.render_widget(
            Paragraph::new("Enter send · PgUp/PgDn scroll · Ctrl+L new chat · Ctrl+C quit")
                .fg(Color::DarkGray),
            help,
        );
    }
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub async fn run(
    base_url: String,
    api_key: String,
    config: AgentConfig,
    prompt: Option<String>,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("The TUI requires a terminal; use -p <prompt> for noninteractive runs");
    }
    let mut app = App::new(config.model.clone());
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (commands, mut requests) = mpsc::unbounded_channel();
    let (completed_tx, mut completed) = mpsc::unbounded_channel();
    let mut session = Session::new(base_url, api_key, config, Output::channel(events_tx));
    // Ratatui's panic hook restores the terminal; the guard also covers ordinary errors.
    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init()?;
    let worker = async move {
        while let Some(command) = requests.recv().await {
            match command {
                Command::Submit(prompt) => {
                    let result = session
                        .submit(prompt)
                        .await
                        .map_err(|error| format!("{error:#}"));
                    if completed_tx.send(result).is_err() {
                        break;
                    }
                }
                Command::Clear => session.clear(),
            }
        }
    };
    if let Some(prompt) = prompt {
        app.input = prompt;
    }
    tokio::select! {
        result = event_loop(&mut terminal, &mut app, commands, &mut events, &mut completed) => result,
        () = worker => bail!("Agent worker stopped"),
    }
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    commands: mpsc::UnboundedSender<Command>,
    events: &mut mpsc::UnboundedReceiver<Event>,
    completed: &mut mpsc::UnboundedReceiver<std::result::Result<(), String>>,
) -> Result<()> {
    let mut input = EventStream::new();
    app.submit(&commands)?;
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { bail!("Agent worker stopped") };
                app.event(event);
            }
            result = completed.recv() => {
                let Some(result) = result else { bail!("Agent worker stopped") };
                while let Ok(event) = events.try_recv() { app.event(event); }
                app.busy = false;
                app.status = "Ready".into();
                if let Err(error) = result {
                    app.append("ERROR", &format!("{error}\nFailed turn removed from history; local tool side effects remain."), Color::Red);
                }
            }
            event = input.next() => {
                let Some(event) = event else { return Ok(()) };
                match event? {
                    TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match key.code {
                                KeyCode::Char('c') => return Ok(()),
                                KeyCode::Char('l') if !app.busy => {
                                    commands.send(Command::Clear).context("Agent worker stopped")?;
                                    app.lines.clear();
                                    app.scroll_back = 0;
                                    app.status = "New conversation".into();
                                }
                                _ => {}
                            }
                            continue;
                        }
                        match key.code {
                            KeyCode::Enter => app.submit(&commands)?,
                            KeyCode::Backspace => {
                                if let Some((index, _)) = app.input.grapheme_indices(true).next_back() {
                                    app.input.truncate(index);
                                }
                            }
                            KeyCode::Char(c) => app.input.push(c),
                            KeyCode::PageUp => app.scroll_back = app.scroll_back.saturating_add(10),
                            KeyCode::PageDown => app.scroll_back = app.scroll_back.saturating_sub(10),
                            _ => {}
                        }
                    }
                    TerminalEvent::Paste(text) => app.input.extend(text.chars().filter(|c| !c.is_control())),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn renders_small_terminals_and_bounds_history() {
        let mut app = App::new("mock".into());
        app.append("TOOL", &"line\n".repeat(MAX_LINES + 10), Color::Blue);
        assert_eq!(app.lines.len(), MAX_LINES);
        for (width, height) in [(80, 24), (10, 4), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
    }

    #[test]
    fn ignores_empty_and_busy_submissions() {
        let mut app = App::new("mock".into());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.submit(&sender).unwrap();
        assert!(receiver.try_recv().is_err());
        app.input = "hello".into();
        app.submit(&sender).unwrap();
        assert!(
            matches!(receiver.try_recv().unwrap(), Command::Submit(prompt) if prompt == "hello")
        );
        app.input = "draft".into();
        app.submit(&sender).unwrap();
        assert_eq!(app.input, "draft");
        assert!(receiver.try_recv().is_err());
    }
}
