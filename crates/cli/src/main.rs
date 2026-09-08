use anyhow::{Context, Result};
use clap::Parser;
use ri_agent::{
    agent::{AgentConfig, Session},
    config::LuaConfig,
    events::Output,
    limits,
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
    about = "A Lua-configurable agent harness with CLI and Ratatui interfaces"
)]
struct Args {
    /// Run a single prompt without the TUI (unless --tui is also supplied).
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// Open the interactive terminal interface.
    #[arg(long)]
    tui: bool,
    /// Trusted Lua configuration; defaults to $XDG_CONFIG_HOME/ri-agent/config.lua.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, env = "MODEL")]
    model: Option<String>,
    #[arg(long, env = "OPENROUTER_BASE_URL")]
    base_url: Option<String>,
    #[arg(long)]
    max_turns: Option<NonZeroUsize>,
    #[arg(long)]
    command_timeout: Option<NonZeroU64>,
    #[arg(long)]
    max_output_bytes: Option<NonZeroUsize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let lua = LuaConfig::load(args.config)?;
    let settings = &lua.settings;
    let base_url = args
        .base_url
        .or_else(|| settings.base_url.clone())
        .unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    let api_key =
        env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set or is invalid")?;
    let config = AgentConfig {
        model: args
            .model
            .or_else(|| settings.model.clone())
            .unwrap_or_else(|| "anthropic/claude-haiku-4.5".into()),
        max_turns: args
            .max_turns
            .or(settings.max_turns)
            .unwrap_or(limits::MAX_TURNS),
        limits: limits::Limits {
            command_timeout: std::time::Duration::from_secs(
                args.command_timeout
                    .or(settings.command_timeout)
                    .map(NonZeroU64::get)
                    .unwrap_or(60),
            ),
            max_output_bytes: args
                .max_output_bytes
                .or(settings.max_output_bytes)
                .unwrap_or(NonZeroUsize::new(32768).unwrap()),
        },
        lua,
    };
    if args.tui || args.prompt.is_none() {
        return ri_agent_tui::run(base_url, api_key, config, args.prompt).await;
    }
    Session::new(base_url, api_key, config, Output::default())
        .submit(args.prompt.unwrap())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_interactive_and_one_shot_modes_and_rejects_zero_limits() {
        assert!(Args::try_parse_from(["agent"]).unwrap().prompt.is_none());
        let args = Args::try_parse_from([
            "agent",
            "-p",
            "hello",
            "--model",
            "test",
            "--max-turns",
            "3",
        ])
        .unwrap();
        assert_eq!(args.model.as_deref(), Some("test"));
        assert_eq!(args.max_turns.unwrap().get(), 3);
        for flag in ["--max-turns", "--command-timeout", "--max-output-bytes"] {
            assert!(Args::try_parse_from(["agent", "-p", "hello", flag, "0"]).is_err());
        }
    }
}
