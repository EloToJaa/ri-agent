use anyhow::{Result, bail};
use ri_agent::config::ReasoningEffort;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Provider(Option<String>),
    Reasoning(Option<ReasoningEffort>),
    Resume(Option<String>),
    Login { manual: bool },
    Help,
    Compact,
}

const COMMANDS: [&str; 7] = [
    "/model",
    "/reasoning",
    "/resume",
    "/login",
    "/help",
    "/provider",
    "/compact",
];

pub fn suggestions(input: &str) -> Vec<&'static str> {
    let prefix = input.split_whitespace().next().unwrap_or_default();
    COMMANDS
        .iter()
        .copied()
        .filter(|command| command.starts_with(prefix))
        .collect()
}

#[cfg(test)]
pub fn complete(input: &str, selected: usize) -> Option<String> {
    suggestions(input)
        .get(selected)
        .map(|command| format!("{command} "))
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
        "/provider" => {
            if let Some(id) = &argument {
                ri_agent::providers::default_model(id)?;
            }
            Ok(SlashCommand::Provider(argument))
        }
        "/reasoning" => Ok(SlashCommand::Reasoning(
            argument
                .map(|value| ReasoningEffort::from_str(&value))
                .transpose()?,
        )),
        "/resume" => Ok(SlashCommand::Resume(argument)),
        "/login" => match argument.as_deref() {
            None => Ok(SlashCommand::Login { manual: false }),
            Some("--manual") => Ok(SlashCommand::Login { manual: true }),
            _ => bail!("Usage: /login [--manual]"),
        },
        "/help" => Ok(SlashCommand::Help),
        "/compact" if argument.is_none() => Ok(SlashCommand::Compact),
        _ => bail!("Unknown command '{command}'. Use /help for available commands"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_commands_as_the_slash_prefix_is_typed() {
        assert_eq!(
            suggestions("/"),
            vec![
                "/model",
                "/reasoning",
                "/resume",
                "/login",
                "/help",
                "/provider",
                "/compact"
            ]
        );
        assert_eq!(suggestions("/rea"), vec!["/reasoning"]);
        assert_eq!(complete("/mod", 0).as_deref(), Some("/model "));
        assert_eq!(complete("/", 2).as_deref(), Some("/resume "));
        assert!(complete("/unknown", 0).is_none());
    }

    #[test]
    fn parses_commands_and_rejects_ambiguous_input() -> Result<()> {
        assert_eq!(parse("/login")?, SlashCommand::Login { manual: false });
        assert_eq!(
            parse("/login --manual")?,
            SlashCommand::Login { manual: true }
        );
        assert!(parse("/login --unknown").is_err());
        assert_eq!(parse("/provider")?, SlashCommand::Provider(None));
        assert_eq!(parse("/compact")?, SlashCommand::Compact);
        assert_eq!(
            parse("/provider openai-codex")?,
            SlashCommand::Provider(Some("openai-codex".into()))
        );
        assert!(parse("/provider invalid").is_err());
        assert_eq!(complete("/pro", 0).as_deref(), Some("/provider "));
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
