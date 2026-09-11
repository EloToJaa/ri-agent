//! Provider-neutral chat and model discovery boundary and provider implementations.
pub mod codex;
pub mod openrouter;
mod sse;
pub use crate::config::ReasoningEffort;
pub use crate::message::Message as ChatMessage;
pub use crate::response::{Message as Completion, ToolCall, ToolCallFunction};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use std::{future::Future, pin::Pin};

/// Connect using environment/saved credentials, preserving the configured `OpenRouter` endpoint.
pub fn connect(id: &str, openrouter_base_url: &str) -> Result<std::sync::Arc<dyn Provider>> {
    use self::{codex::Codex, openrouter::OpenRouter};
    use crate::credentials;
    match id {
        "openrouter" => Ok(std::sync::Arc::new(OpenRouter::new(
            openrouter_base_url,
            credentials::resolve(openrouter_base_url)?,
        )?)),
        "openai-codex" => {
            let auth = credentials::load_codex(&credentials::default_path()?)?.context(
                "No OpenAI Codex credentials found. Run 'ri login --provider openai-codex'",
            )?;
            Ok(std::sync::Arc::new(Codex::new(
                codex::DEFAULT_BASE_URL,
                auth,
            )?))
        }
        value => bail!("Unknown provider '{value}'; expected openrouter or openai-codex"),
    }
}

/// Use a currently advertised model for Codex instead of a potentially retired slug.
pub async fn initial_model(provider: &dyn Provider) -> Result<String> {
    let fallback = default_model(provider.id())?;
    if provider.id() != "openai-codex" {
        return Ok(fallback.into());
    }
    let models = provider.models().await?;
    preferred_model(&models, fallback)
}

fn preferred_model(models: &[Model], preferred: &str) -> Result<String> {
    models
        .iter()
        .find(|model| model.id == preferred)
        .or_else(|| models.first())
        .map(|model| model.id.clone())
        .context("Provider catalog contains no models")
}

pub fn default_model(id: &str) -> Result<&'static str> {
    match id {
        "openrouter" => Ok("anthropic/claude-haiku-4.5"),
        "openai-codex" => Ok("gpt-5.1-codex"),
        value => bail!("Unknown provider '{value}'; expected openrouter or openai-codex"),
    }
}

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Clone, Copy)]
pub struct CompletionRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub model: &'a str,
    pub tools: &'a [serde_json::Value],
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Implementations translate the shared protocol into their upstream API format.
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn models(&self) -> ProviderFuture<'_, Vec<Model>>;
    /// Advertised context size, when a catalog has been loaded.
    fn context_window(&self, _model: &str) -> Option<usize> {
        None
    }
    fn complete<'a>(&'a self, request: CompletionRequest<'a>) -> ProviderFuture<'a, Completion>;
    /// Emit provisional text deltas; return the authoritative complete message.
    /// Tools must only execute after this future succeeds.
    fn complete_stream<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        _output: &'a crate::events::Output,
    ) -> ProviderFuture<'a, Completion> {
        self.complete(request)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_length: Option<usize>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<ReasoningCapabilities>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningCapabilities {
    #[serde(default)]
    pub mandatory: bool,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub supported_efforts: SupportedEfforts,
}

/// `OpenRouter` distinguishes omitted effort metadata from explicit null (all efforts).
#[derive(Debug, Clone, Default)]
pub enum SupportedEfforts {
    #[default]
    Unavailable,
    All,
    Levels(Vec<String>),
}

impl<'de> Deserialize<'de> for SupportedEfforts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<Vec<String>>::deserialize(deserializer)?.map_or(Self::All, Self::Levels))
    }
}

impl Model {
    pub fn supports_tools(&self) -> bool {
        self.supported_parameters
            .iter()
            .any(|parameter| parameter == "tools")
            && !self.id.ends_with(":batch")
    }

    pub fn efforts(&self) -> Vec<ReasoningEffort> {
        let Some(reasoning) = &self.reasoning else {
            return if self
                .supported_parameters
                .iter()
                .any(|parameter| parameter == "reasoning" || parameter == "reasoning_effort")
            {
                ReasoningEffort::ALL.to_vec()
            } else {
                Vec::new()
            };
        };
        ReasoningEffort::ALL
            .into_iter()
            .filter(|effort| {
                if reasoning.mandatory && *effort == ReasoningEffort::None {
                    return false;
                }
                match &reasoning.supported_efforts {
                    SupportedEfforts::Unavailable => false,
                    SupportedEfforts::All => true,
                    SupportedEfforts::Levels(levels) => {
                        levels.iter().any(|level| level == effort.as_str())
                    }
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_an_advertised_default_when_the_preferred_model_is_retired() -> Result<()> {
        let models: Vec<Model> = serde_json::from_value(json!([
            {"id":"current-model"}, {"id":"preferred-model"}
        ]))?;
        assert_eq!(
            preferred_model(&models, "preferred-model")?,
            "preferred-model"
        );
        assert_eq!(preferred_model(&models, "retired-model")?, "current-model");
        assert!(preferred_model(&[], "retired-model").is_err());
        Ok(())
    }

    #[test]
    fn distinguishes_missing_null_and_restricted_efforts() -> Result<()> {
        for (metadata, expected) in [
            (json!({}), Vec::new()),
            (
                json!({"supported_efforts": null}),
                ReasoningEffort::ALL.to_vec(),
            ),
            (
                json!({"mandatory":true,"supported_efforts":["none","high","low","future"]}),
                vec![ReasoningEffort::Low, ReasoningEffort::High],
            ),
        ] {
            let model: Model = serde_json::from_value(json!({"id":"test","reasoning":metadata}))?;
            assert_eq!(model.efforts(), expected);
        }
        let model: Model = serde_json::from_value(json!({"id":"plain"}))?;
        assert!(model.efforts().is_empty());
        Ok(())
    }
}
