use anyhow::{Context, Result, bail};
use clap::Parser;
use ri_agent::{
    agent::{AgentConfig, Session},
    config::{LuaConfig, ReasoningEffort},
    events::Output,
    limits,
    providers::{self, openrouter},
    sessions::SessionStore,
};
mod login;
mod openai_login;

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
#[allow(clippy::struct_excessive_bools)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
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
    /// Model provider: openrouter or openai-codex.
    #[arg(long, default_value = "openrouter")]
    provider: String,
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
    /// Emit newline-delimited JSON events instead of human-readable output.
    #[arg(long)]
    json: bool,
    /// Block Bash, Write, Edit, and custom tools for this invocation.
    #[arg(long)]
    read_only: bool,
    /// Ask before Bash, Write, Edit, and custom tool calls (requires a terminal).
    #[arg(long)]
    ask: bool,
    /// Disable automatic AGENTS.md discovery for this invocation.
    #[arg(long)]
    no_project_instructions: bool,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Authenticate this installation with `OpenRouter`.
    Login {
        /// Provider to authenticate: openrouter or openai-codex.
        #[arg(long, default_value = "openrouter")]
        provider: String,
        /// Paste an existing key instead of opening a browser.
        #[arg(long)]
        api_key: bool,
        /// Paste the browser's redirected callback URL instead of using localhost callback delivery.
        #[arg(long)]
        manual: bool,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(Command::Login {
        provider,
        api_key,
        manual,
    }) = args.command
    {
        return match provider.as_str() {
            "openrouter" => login::run(api_key, manual).await,
            "openai-codex" if !api_key => Box::pin(openai_login::run(manual)).await,
            "openai-codex" => bail!(
                "OpenAI Codex login does not support --api-key; use browser login or --manual"
            ),
            _ => bail!("Unknown provider '{provider}'; expected openrouter or openai-codex"),
        };
    }
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
    let provider = providers::connect(&args.provider, &base_url)?;
    let model = match args.model.or_else(|| settings.model.clone()) {
        Some(model) => model,
        None => providers::initial_model(provider.as_ref()).await?,
    };
    let config = AgentConfig {
        model,
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
            read_only: args.read_only,
            ask: args.ask,
        },
        lua,
    };
    let skills = ri_agent::skills::Skills::discover(&[
        ri_agent::config::harness_directory()?.join("skills"),
        std::env::current_dir()?.join(".ri/skills"),
    ]);
    for warning in &skills.warnings {
        eprintln!("Skill warning: {warning}");
    }
    let output = if args.json {
        Output::json()
    } else {
        Output::default()
    };
    let mut session = Session::new(provider, config, output).with_skills(skills);
    if !args.no_project_instructions {
        session = session.with_project_instructions(env::current_dir()?);
    }
    if let Some(store) = store {
        session = session.with_store(store);
    }
    let interrupted = resume(&mut session, args.resume).await?;
    if args.tui || args.prompt.is_none() {
        return ri_agent_tui::run_with_base_url(session, args.prompt, interrupted, &base_url).await;
    }
    if interrupted {
        eprintln!(
            "Interrupted run: restored the last completed prompt. Local tool effects may remain."
        );
    }
    if let Some(effort) = session.selection().reasoning_effort {
        let models = session.provider().models().await?;
        let selection = session.selection();
        let model = models
            .iter()
            .find(|model| model.id == selection.model)
            .with_context(|| {
                format!(
                    "Selected model '{}' is not in the {} catalog; check --model or MODEL",
                    selection.model,
                    session.provider().name()
                )
            })?;
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
        let login =
            Args::try_parse_from(["ri", "login", "--provider", "openai-codex", "--manual"])?;
        assert!(
            matches!(login.command, Some(Command::Login { provider, manual: true, api_key: false }) if provider == "openai-codex")
        );
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
