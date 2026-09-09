//! `OpenAI Codex` provider using `ChatGPT` OAuth credentials.
use crate::{
    credentials::CodexCredentials,
    provider::{
        Completion, CompletionRequest, Model, Provider, ProviderFuture, ReasoningCapabilities,
        SupportedEfforts,
    },
    response::{ToolCall, ToolCallFunction},
};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";

pub struct Codex {
    client: Client,
    base_url: String,
    credentials: CodexCredentials,
}

impl Codex {
    pub fn new(base_url: &str, credentials: CodexCredentials) -> Result<Self> {
        let url = reqwest::Url::parse(base_url).context("Invalid Codex API URL")?;
        if url.scheme() != "https" || url.host_str() != Some("chatgpt.com") {
            bail!("Codex OAuth credentials can only be sent to https://chatgpt.com");
        }
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_mins(5))
                .build()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            credentials,
        })
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(self.credentials.access_token.expose_secret())
            .header("chatgpt-account-id", &self.credentials.account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "ri")
    }
}

#[derive(Deserialize)]
struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    slug: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    priority: usize,
}

#[derive(Deserialize)]
struct ReasoningLevel {
    effort: String,
}

fn catalog_models(mut catalog: Catalog) -> Result<Vec<Model>> {
    catalog.models.sort_by_key(|entry| entry.priority);
    let mut seen = std::collections::HashSet::new();
    let models = catalog
        .models
        .into_iter()
        .filter(|entry| seen.insert(entry.slug.clone()))
        .map(|entry| {
            let levels: Vec<String> = entry
                .supported_reasoning_levels
                .into_iter()
                .map(|level| level.effort)
                .collect();
            Model {
                id: entry.slug,
                name: entry.display_name,
                supported_parameters: vec!["tools".into(), "reasoning".into()],
                reasoning: Some(ReasoningCapabilities {
                    mandatory: !levels.iter().any(|level| level == "none"),
                    default_effort: entry.default_reasoning_level,
                    supported_efforts: SupportedEfforts::Levels(levels),
                }),
            }
        })
        .collect::<Vec<_>>();
    if models.is_empty() {
        bail!("OpenAI Codex catalog contains no models");
    }
    Ok(models)
}

fn catalog_request(client: &Client, base_url: &str) -> reqwest::RequestBuilder {
    client
        .get(format!("{base_url}/models"))
        .query(&[("client_version", env!("CARGO_PKG_VERSION"))])
}

impl Provider for Codex {
    fn id(&self) -> &'static str {
        "openai-codex"
    }
    fn name(&self) -> &'static str {
        "OpenAI Codex"
    }

    fn models(&self) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async {
            let catalog = self
                .request(catalog_request(&self.client, &self.base_url))
                .send()
                .await
                .context("Fetching OpenAI Codex model catalog")?
                .error_for_status()
                .context("OpenAI Codex model catalog request failed")?
                .json::<Catalog>()
                .await
                .context("Invalid OpenAI Codex model catalog")?;
            catalog_models(catalog)
        })
    }

    fn complete<'a>(&'a self, request: CompletionRequest<'a>) -> ProviderFuture<'a, Completion> {
        Box::pin(async move {
            let input = request.messages.iter().flat_map(|message| match message {
                crate::provider::ChatMessage::User { content } => vec![json!({"type":"message","role":"user","content":[{"type":"input_text","text":content}]})],
                crate::provider::ChatMessage::Assistant { content, tool_calls, .. } => {
                    let mut items = content.as_ref().map_or_else(Vec::new, |text| vec![json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})]);
                    items.extend(tool_calls.iter().map(|call| json!({"type":"function_call","call_id":call.id,"name":call.function.name,"arguments":call.function.arguments.as_deref().unwrap_or("{}") }))); items
                }
                crate::provider::ChatMessage::Tool { content, tool_call_id } => vec![json!({"type":"function_call_output","call_id":tool_call_id,"output":content})],
            }).collect::<Vec<_>>();
            let tools = request
                .tools
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function")?;
                    Some(json!({
                        "type": "function",
                        "name": function.get("name")?,
                        "description": function.get("description"),
                        "parameters": function.get("parameters")?,
                        "strict": false
                    }))
                })
                .collect::<Vec<_>>();
            let mut body = json!({"model": request.model, "input": input, "tools": tools, "store": false, "stream": true});
            if let Some(effort) = request.reasoning_effort
                && let Some(object) = body.as_object_mut()
            {
                object.insert("reasoning".into(), json!({"effort": effort}));
            }
            let payload = self
                .request(self.client.post(format!("{}/responses", self.base_url)))
                .json(&body)
                .send()
                .await
                .context("Requesting OpenAI Codex completion")?
                .error_for_status()
                .context("OpenAI Codex completion failed")?
                .text()
                .await
                .context("Reading OpenAI Codex response stream")?;
            let completed = payload
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .find(|event| {
                    event.get("type").and_then(Value::as_str) == Some("response.completed")
                })
                .and_then(|event| event.get("response").cloned())
                .context("OpenAI Codex response stream ended without a completed response")?;
            parse_response(&completed)
        })
    }
}

fn parse_response(value: &Value) -> Result<Completion> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .context("Codex response contains no output")?;
    let mut text = Vec::new();
    let mut reasoning = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    text.extend(
                        content
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .map(str::to_owned),
                    );
                }
            }
            Some("reasoning") => {
                if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                    reasoning.extend(
                        summary
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .map(str::to_owned),
                    );
                }
            }
            Some("function_call") => tool_calls.push(ToolCall {
                id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .context("Codex tool call has no id")?
                    .to_owned(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .context("Codex tool call has no name")?
                        .to_owned(),
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            }),
            _ => {}
        }
    }
    Ok(Completion {
        content: (!text.is_empty()).then(|| text.join("\n")),
        reasoning: (!reasoning.is_empty()).then(|| reasoning.join("\n")),
        reasoning_details: None,
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReasoningEffort;

    #[test]
    fn catalog_request_includes_client_version() -> Result<()> {
        let request = catalog_request(&Client::new(), DEFAULT_BASE_URL).build()?;
        assert_eq!(request.url().path(), "/backend-api/codex/models");
        assert!(
            request
                .url()
                .query_pairs()
                .any(|(key, value)| key == "client_version" && value == env!("CARGO_PKG_VERSION"))
        );
        Ok(())
    }

    #[test]
    fn maps_codex_catalog_reasoning_levels_and_default() -> Result<()> {
        let models = catalog_models(serde_json::from_str(include_str!(
            "../tests/fixtures/codex_models.json"
        ))?)?;
        let model = models.first().context("Missing model")?;
        assert_eq!(
            model.efforts(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh
            ]
        );
        assert_eq!(
            model
                .reasoning
                .as_ref()
                .and_then(|r| r.default_effort.as_deref()),
            Some("medium")
        );
        let models = catalog_models(serde_json::from_value(json!({"models": [
            {"slug":"optional", "supported_reasoning_levels":[{"effort":"none"},{"effort":"high"},{"effort":"future"}]},
            {"slug":"unknown"}
        ]}))?)?;
        assert_eq!(
            models.first().context("Missing model")?.efforts(),
            vec![ReasoningEffort::None, ReasoningEffort::High]
        );
        assert!(models.get(1).context("Missing model")?.efforts().is_empty());
        assert!(catalog_models(serde_json::from_str(r#"{"models":[]}"#)?).is_err());
        Ok(())
    }
}
