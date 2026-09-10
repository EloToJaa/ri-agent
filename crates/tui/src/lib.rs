mod app;
mod commands;
mod login;
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
    providers::{self, Provider},
    sessions::SessionSummary,
};
use std::io::{self, IsTerminal};
use tokio::sync::mpsc;

pub(crate) enum Command {
    Submit(String, ri_agent::cancellation::Cancellation),
    Select(Selection),
    Provider(String),
    Resume(String),
    ListSessions,
    Clear,
    Login { manual: bool },
    Compact,
}

enum Outcome {
    LoginRequested {
        manual: bool,
    },
    ProviderChanged {
        id: String,
        selection: Selection,
        provider: std::sync::Arc<dyn Provider>,
    },
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

async fn execute(command: Command, session: &mut Session, base_url: &str) -> Result<Outcome> {
    match command {
        Command::Submit(prompt, cancellation) => {
            session.submit_cancellable(prompt, &cancellation).await?;
        }
        Command::Compact => {
            let stats = session.compact().await?;
            return Ok(Outcome::Notice(format!(
                "Context compacted: {} messages, approximately {} tokens.",
                stats.messages, stats.approximate_tokens
            )));
        }
        Command::Select(selection) => session.select(selection).await?,
        Command::Provider(id) => {
            if id != session.provider().id() {
                let provider = providers::connect(&id, base_url)?;
                let model = providers::initial_model(provider.as_ref()).await?;
                session.switch_provider(provider, model).await?;
                return Ok(Outcome::ProviderChanged {
                    id: session.id().to_owned(),
                    selection: session.selection(),
                    provider: session.provider(),
                });
            }
        }
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
        // Only the frontend can safely hand ownership of its terminal to login.
        Command::Login { manual } => return Ok(Outcome::LoginRequested { manual }),
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

pub async fn run(session: Session, prompt: Option<String>, interrupted: bool) -> Result<()> {
    run_with_base_url(
        session,
        prompt,
        interrupted,
        ri_agent::providers::openrouter::DEFAULT_BASE_URL,
    )
    .await
}

pub async fn run_with_base_url(
    mut session: Session,
    prompt: Option<String>,
    interrupted: bool,
    base_url: &str,
) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("The TUI requires a terminal; use -p <prompt> for noninteractive runs");
    }
    let mut app = App::new(&session, interrupted);
    let mut provider = session.provider();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (commands, mut requests) = mpsc::unbounded_channel();
    let (completed_tx, mut completed) = mpsc::unbounded_channel();
    session.set_output(Output::channel(events_tx));
    let (refresh, mut refresh_requests) = mpsc::unbounded_channel::<std::sync::Arc<dyn Provider>>();
    let (catalog_tx, mut catalogs) = mpsc::unbounded_channel();
    refresh.send(provider.clone())?;
    let catalog_worker = async move {
        while let Some(provider) = refresh_requests.recv().await {
            let result = provider
                .models()
                .await
                .map_err(|error| format!("{error:#}"));
            if catalog_tx.send((provider.id(), result)).is_err() {
                break;
            }
        }
    };
    let worker = async move {
        while let Some(command) = requests.recv().await {
            let result = execute(command, &mut session, base_url)
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
                    // Coalesce queued deltas before redrawing, while still yielding to input.
                    for _ in 0..127 {
                        let Ok(event) = events.try_recv() else { break; };
                        app.event(event);
                    }
                }
                result = completed.recv() => {
                    while let Ok(event) = events.try_recv() { app.event(event); }
                    let result = match result.context("Agent worker stopped")? {
                        Ok(Outcome::LoginRequested { manual }) => {
                            app.finish(Ok(Outcome::LoginRequested { manual }));
                            // Stop the event reader before the child inherits stdin. No TUI
                            // input or drawing happens until the terminal has been restored.
                            drop(input);
                            let result = login::run(&mut terminal, provider.id(), manual).await?;
                            input = EventStream::new();
                            result.map(|()| Outcome::Notice(format!(
                                "{} login saved. Restart ri to use the new credential.", provider.name()
                            ))).map_err(|error| format!("{error:#}"))
                        }
                        result => result,
                    };
                    if let Ok(Outcome::ProviderChanged { provider: selected, .. }) = &result {
                        provider = selected.clone();
                        refresh.send(provider.clone()).context("Catalog worker stopped")?;
                    }
                    app.finish(result);
                    app.continue_pending(&commands)?;
                }
                catalog = catalogs.recv() => {
                    let (id, result) = catalog.context("Catalog worker stopped")?;
                    if id == provider.id() {
                        app.catalog(result);
                        if initial_prompt { initial_prompt = false; app.submit(&commands)?; }
                    }
                }
                event = input.next() => {
                    if handle_input(event, &mut app, &commands, &refresh, &provider)? { return Ok(()); }
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
    refresh: &mpsc::UnboundedSender<std::sync::Arc<dyn Provider>>,
    provider: &std::sync::Arc<dyn Provider>,
) -> Result<bool> {
    let Some(event) = event else { return Ok(true) };
    match event? {
        TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
            if key.code == KeyCode::F(5) && !app.catalog_loading {
                app.catalog_loading = true;
                refresh
                    .send(provider.clone())
                    .context("Catalog worker stopped")?;
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
