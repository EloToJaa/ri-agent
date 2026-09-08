use anyhow::{Context, Result, bail};
use clap::Parser;
use ri_agent::{
    agent::{AgentConfig, Session},
    config::{LuaConfig, ReasoningEffort},
    events::Output,
    limits, openrouter,
    sessions::SessionStore,
};
use std::{
    env,
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "OpenRouter agent harness with Lua configuration and a Ratatui interface"
)]
struct Args {
    /// Run a single prompt without the TUI (unless --tui is also supplied).
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    #[arg(long)]
    tui: bool,
    /// Trusted Lua configuration; defaults to ~/.ri/config.lua.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, env = "MODEL")]
    model: Option<String>,
    /// Reasoning effort: none, minimal, low, medium, high, xhigh, or max.
    #[arg(long)]
    reasoning: Option<ReasoningEffort>,
    /// Override the `OpenRouter` endpoint (also useful for local mock servers).
    #[arg(long, env = "OPENROUTER_BASE_URL")]
    base_url: Option<String>,
    #[arg(long)]
    max_turns: Option<NonZeroUsize>,
    #[arg(long)]
    command_timeout: Option<NonZeroU64>,
    #[arg(long)]
    max_output_bytes: Option<NonZeroUsize>,
    /// Resume a session ID, or the latest session in this directory when omitted.
    #[arg(long, num_args = 0..=1, default_missing_value = "latest", conflicts_with = "no_save")]
    resume: Option<String>,
    /// List saved sessions for the current working directory and exit.
    #[arg(long, conflicts_with_all = ["no_save", "resume", "prompt", "tui"])]
    sessions: bool,
    /// Override the `SQLite` session database path.
    #[arg(long, conflicts_with = "no_save")]
    session_db: Option<PathBuf>,
    /// Do not save conversation or tool results to disk.
    #[arg(long)]
    no_save: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let store = if args.no_save {
        None
    } else {
        Some(SessionStore::open(
            args.session_db
                .map_or_else(SessionStore::default_path, Ok)?,
            &env::current_dir()?,
        )?)
    };
    if args.sessions {
        for saved in store
            .as_ref()
            .context("Session persistence is disabled")?
            .list()
            .await?
        {
            println!(
                "{}  {}  {}  {}",
                saved.id, saved.updated_at, saved.model, saved.title
            );
        }
        return Ok(());
    }
    let lua = LuaConfig::load(args.config)?;
    let settings = &lua.settings;
    let base_url = args
        .base_url
        .or_else(|| settings.base_url.clone())
        .unwrap_or_else(|| openrouter::DEFAULT_BASE_URL.into());
    let api_key =
        env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set or is invalid")?;
    let config = AgentConfig {
        model: args
            .model
            .or_else(|| settings.model.clone())
            .unwrap_or_else(|| "anthropic/claude-haiku-4.5".into()),
        reasoning_effort: args.reasoning.or(settings.reasoning_effort),
        max_turns: args
            .max_turns
            .or(settings.max_turns)
            .unwrap_or(limits::MAX_TURNS),
        limits: limits::Limits {
            command_timeout: std::time::Duration::from_secs(
                args.command_timeout
                    .or(settings.command_timeout)
                    .map_or(60, NonZeroU64::get),
            ),
            max_output_bytes: args
                .max_output_bytes
                .or(settings.max_output_bytes)
                .unwrap_or_else(|| limits::Limits::default().max_output_bytes),
        },
        lua,
    };
    let mut session = Session::new(base_url.clone(), api_key, config, Output::default());
    if let Some(store) = store {
        session = session.with_store(store);
    }
    let interrupted = resume(&mut session, args.resume).await?;
    if args.tui || args.prompt.is_none() {
        return ri_agent_tui::run(base_url, session, args.prompt, interrupted).await;
    }
    if interrupted {
        eprintln!(
            "Interrupted run: restored the last completed prompt. Local tool effects may remain."
        );
    }
    if let Some(effort) = session.selection().reasoning_effort {
        let models = openrouter::models(&base_url).await?;
        let selection = session.selection();
        let model = models
            .iter()
            .find(|model| model.id == selection.model)
            .context("Selected model is not in the OpenRouter tool-capable catalog")?;
        if !model.efforts().contains(&effort) {
            bail!(
                "Model {} does not advertise reasoning effort {effort}",
                model.id
            );
        }
    }
    if session.store().is_some() {
        eprintln!("Session: {}", session.id());
    }
    session.submit(args.prompt.context("Missing prompt")?).await
}

async fn resume(session: &mut Session, id: Option<String>) -> Result<bool> {
    let Some(id) = id else { return Ok(false) };
    if id != "latest" {
        return session.resume(&id).await;
    }
    let id = session
        .store()
        .context("Session persistence is disabled")?
        .list()
        .await?
        .first()
        .context("No saved sessions in this directory")?
        .id
        .clone();
    session.resume(&id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_modes_selectors_and_rejects_invalid_limits() -> Result<()> {
        assert!(Args::try_parse_from(["agent"])?.prompt.is_none());
        let args = Args::try_parse_from([
            "agent",
            "-p",
            "hello",
            "--model",
            "test",
            "--max-turns",
            "3",
            "--reasoning",
            "high",
        ])?;
        assert_eq!(args.model.as_deref(), Some("test"));
        assert_eq!(args.max_turns.map(NonZeroUsize::get), Some(3));
        assert_eq!(args.reasoning, Some(ReasoningEffort::High));
        assert_eq!(
            Args::try_parse_from(["agent", "--resume"])?
                .resume
                .as_deref(),
            Some("latest")
        );
        for flag in ["--max-turns", "--command-timeout", "--max-output-bytes"] {
            assert!(Args::try_parse_from(["agent", "-p", "hello", flag, "0"]).is_err());
        }
        assert!(Args::try_parse_from(["agent", "--reasoning", "invalid"]).is_err());
        assert!(Args::try_parse_from(["agent", "--resume", "--no-save"]).is_err());
        Ok(())
    }
}
