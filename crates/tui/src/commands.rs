use anyhow::{Result, bail};
use ri_agent::config::ReasoningEffort;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Reasoning(Option<ReasoningEffort>),
    Resume(Option<String>),
    Login,
    Help,
}

pub fn parse(input: &str) -> Result<SlashCommand> {
    let mut parts = input.split_whitespace();
    let command = parts.next().unwrap_or_default();
    let argument = parts.next().map(str::to_owned);
    if parts.next().is_some() {
        bail!("Slash commands accept at most one argument");
    }
    match command {
        "/model" => Ok(SlashCommand::Model(argument)),
        "/reasoning" => Ok(SlashCommand::Reasoning(
            argument
                .map(|value| ReasoningEffort::from_str(&value))
                .transpose()?,
        )),
        "/resume" => Ok(SlashCommand::Resume(argument)),
        "/login" => Ok(SlashCommand::Login),
        "/help" => Ok(SlashCommand::Help),
        _ => bail!("Unknown command '{command}'. Use /help for available commands"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_rejects_ambiguous_input() -> Result<()> {
        assert_eq!(parse("/model")?, SlashCommand::Model(None));
        assert_eq!(
            parse("/model qwen")?,
            SlashCommand::Model(Some("qwen".into()))
        );
        assert_eq!(
            parse("/reasoning high")?,
            SlashCommand::Reasoning(Some(ReasoningEffort::High))
        );
        assert_eq!(
            parse("/resume session-1")?,
            SlashCommand::Resume(Some("session-1".into()))
        );
        assert!(parse("/model one two").is_err());
        assert!(parse("/reasoning invalid").is_err());
        assert!(parse("/unknown").is_err());
        Ok(())
    }
}
