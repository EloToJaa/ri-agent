//! OpenRouter catalog and capability metadata. No other providers are supported.
pub use crate::config::ReasoningEffort;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

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

/// OpenRouter distinguishes omitted effort metadata from explicit null (all efforts).
#[derive(Debug, Clone, Default)]
pub enum SupportedEfforts {
    #[default]
    Unavailable,
    All,
    Levels(Vec<String>),
}

impl<'de> Deserialize<'de> for SupportedEfforts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<Vec<String>>::deserialize(deserializer)? {
            Some(levels) => Self::Levels(levels),
            None => Self::All,
        })
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

/// Fetches the public catalog without sending credentials; failures never replace a cached catalog.
pub async fn models(base_url: &str) -> Result<Vec<Model>> {
    #[derive(Deserialize)]
    struct Catalog {
        data: Vec<Model>,
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .send()
        .await
        .context("Fetching OpenRouter model catalog")?
        .error_for_status()
        .context("OpenRouter model catalog request failed")?;
    let mut models = response
        .json::<Catalog>()
        .await
        .context("Invalid OpenRouter model catalog")?
        .data;
    models.retain(Model::supports_tools);
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    if models.is_empty() {
        bail!("OpenRouter catalog contains no tool-capable models");
    }
    Ok(models)
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
