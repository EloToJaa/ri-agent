mod app;
mod commands;
mod picker;

use anyhow::{Context, Result, bail};
use app::App;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TerminalEvent, EventStream, KeyCode,
    KeyEventKind,
};
use futures_util::StreamExt;
use ri_agent::{
    agent::{Selection, Session},
    events::{Event, Output},
    sessions::SessionSummary,
};
use std::io::{self, IsTerminal};
use tokio::sync::mpsc;

pub(crate) enum Command {
    Submit(String),
    Select(Selection),
    Resume(String),
    ListSessions,
    Clear,
    Login,
}

enum Outcome {
    Updated {
        id: String,
        selection: Selection,
    },
    Loaded {
        id: String,
        selection: Selection,
        history: Vec<Event>,
        interrupted: bool,
    },
    Sessions(Vec<SessionSummary>),
    Notice(String),
}

async fn execute(command: Command, session: &mut Session) -> Result<Outcome> {
    match command {
        Command::Submit(prompt) => session.submit(prompt).await?,
        Command::Select(selection) => session.select(selection).await?,
        Command::Resume(id) => {
            let interrupted = session.resume(&id).await?;
            return Ok(Outcome::Loaded {
                id: session.id().to_owned(),
                selection: session.selection(),
                history: session.history(),
                interrupted,
            });
        }
        Command::ListSessions => {
            return Ok(Outcome::Sessions(
                session
                    .store()
                    .context("Session persistence is disabled")?
                    .list()
                    .await?,
            ));
        }
        Command::Login => {
            let status = std::process::Command::new(
                std::env::current_exe().context("Finding ri executable")?,
            )
            .arg("login")
            .arg("--provider")
            .arg(session.provider().id())
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Running ri login")?;
            if !status.success() {
                bail!("ri login exited with {status}");
            }
            return Ok(Outcome::Notice(format!(
                "{} login saved. Restart ri to use the new credential.",
                session.provider().name()
            )));
        }
        Command::Clear => {
            session.clear();
            return Ok(Outcome::Loaded {
                id: session.id().to_owned(),
                selection: session.selection(),
                history: Vec::new(),
                interrupted: false,
            });
        }
    }
    Ok(Outcome::Updated {
        id: session.id().to_owned(),
        selection: session.selection(),
    })
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}

pub async fn run(mut session: Session, prompt: Option<String>, interrupted: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("The TUI requires a terminal; use -p <prompt> for noninteractive runs");
    }
    let mut app = App::new(&session, interrupted);
    let provider = session.provider();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (commands, mut requests) = mpsc::unbounded_channel();
    let (completed_tx, mut completed) = mpsc::unbounded_channel();
    session.set_output(Output::channel(events_tx));
    let (refresh, mut refresh_requests) = mpsc::unbounded_channel();
    let (catalog_tx, mut catalogs) = mpsc::unbounded_channel();
    refresh.send(())?;
    let catalog_worker = async move {
        while refresh_requests.recv().await.is_some() {
            let result = provider
                .models()
                .await
                .map_err(|error| format!("{error:#}"));
            if catalog_tx.send(result).is_err() {
                break;
            }
        }
    };
    let worker = async move {
        while let Some(command) = requests.recv().await {
            let result = execute(command, &mut session)
                .await
                .map_err(|error| format!("{error:#}"));
            if completed_tx.send(result).is_err() {
                break;
            }
        }
    };
    // The guard covers errors; Ratatui also installs a terminal-restoring panic hook.
    let _restore = RestoreTerminal;
    let mut terminal = ratatui::try_init()?;
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    let mut initial_prompt = prompt.is_some();
    if let Some(prompt) = prompt {
        app.input = prompt;
    }
    let frontend = async {
        let mut input = EventStream::new();
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            tokio::select! {
                result = app.wait_files() => {
                    app.files_ready(result);
                }
                event = events.recv() => {
                    app.event(event.context("Agent worker stopped")?);
                }
                result = completed.recv() => {
                    while let Ok(event) = events.try_recv() { app.event(event); }
                    app.finish(result.context("Agent worker stopped")?);
                }
                catalog = catalogs.recv() => {
                    app.catalog(catalog.context("Catalog worker stopped")?);
                    if initial_prompt { initial_prompt = false; app.submit(&commands)?; }
                }
                event = input.next() => {
                    if handle_input(event, &mut app, &commands, &refresh)? { return Ok(()); }
                }
            }
        }
    };
    // Dropping a running worker marks its SQLite checkpoint interrupted. It never replays tools.
    tokio::select! {
        result = frontend => result,
        () = worker => bail!("Agent worker stopped"),
        () = catalog_worker => bail!("Catalog worker stopped"),
    }
}

fn handle_input(
    event: Option<io::Result<TerminalEvent>>,
    app: &mut App,
    commands: &mpsc::UnboundedSender<Command>,
    refresh: &mpsc::UnboundedSender<()>,
) -> Result<bool> {
    let Some(event) = event else { return Ok(true) };
    match event? {
        TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
            if key.code == KeyCode::F(5) && !app.catalog_loading {
                app.catalog_loading = true;
                refresh.send(()).context("Catalog worker stopped")?;
                return Ok(false);
            }
            app.key(key, commands)
        }
        TerminalEvent::Paste(text) => {
            app.paste(&text);
            Ok(false)
        }
        _ => Ok(false),
    }
}
