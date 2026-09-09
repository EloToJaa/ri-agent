//! Provider-neutral chat and model discovery boundary.
pub use crate::config::ReasoningEffort;
pub use crate::message::Message as ChatMessage;
pub use crate::response::{Message as Completion, ToolCall, ToolCallFunction};
use anyhow::Result;
use serde::{Deserialize, Deserializer};
use std::{future::Future, pin::Pin};

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

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
    fn complete<'a>(&'a self, request: CompletionRequest<'a>) -> ProviderFuture<'a, Completion>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: String,
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
            return Vec::new();
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
