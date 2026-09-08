mod agent;
mod limits;
mod message;
mod response;
mod response_processor;
mod tools;

use anyhow::{Context, Result};
use async_openai::{Client, config::OpenAIConfig};
use clap::Parser;
use std::{
    env,
    num::{NonZeroU64, NonZeroUsize},
};

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(short = 'p', long)]
    prompt: String,
    #[arg(long, env = "MODEL", default_value = "anthropic/claude-haiku-4.5")]
    model: String,
    #[arg(long, default_value_t = limits::MAX_TURNS)]
    max_turns: NonZeroUsize,
    #[arg(long, default_value = "60")]
    command_timeout: NonZeroU64,
    #[arg(long, default_value = "32768")]
    max_output_bytes: NonZeroUsize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let base_url = env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());

    let api_key =
        env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set or is invalid")?;

    let config = OpenAIConfig::new()
        .with_api_base(base_url)
        .with_api_key(api_key);

    let client = Client::with_config(config);

    let limits = limits::Limits {
        command_timeout: std::time::Duration::from_secs(args.command_timeout.get()),
        max_output_bytes: args.max_output_bytes,
    };

    agent::run(
        &client,
        &agent::AgentConfig {
            model: args.model,
            max_turns: args.max_turns,
            limits,
        },
        args.prompt,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_explicit_configuration_and_rejects_zero_limits() {
        let args = Args::try_parse_from([
            "agent",
            "-p",
            "hello",
            "--model",
            "test",
            "--max-turns",
            "3",
            "--command-timeout",
            "2",
            "--max-output-bytes",
            "100",
        ])
        .unwrap();
        assert_eq!(args.model, "test");
        assert_eq!(args.max_turns.get(), 3);
        assert_eq!(args.command_timeout.get(), 2);
        assert_eq!(args.max_output_bytes.get(), 100);
        for flag in ["--max-turns", "--command-timeout", "--max-output-bytes"] {
            assert!(Args::try_parse_from(["agent", "-p", "hello", flag, "0"]).is_err());
        }
    }
}
