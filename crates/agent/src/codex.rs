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

impl Provider for Codex {
    fn id(&self) -> &'static str {
        "openai-codex"
    }
    fn name(&self) -> &'static str {
        "OpenAI Codex"
    }

    fn models(&self) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async {
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
                supported_reasoning_levels: Vec<ReasoningLevel>,
            }
            #[derive(Deserialize)]
            struct ReasoningLevel {
                effort: String,
            }
            let catalog = self
                .request(self.client.get(format!("{}/models", self.base_url)))
                .send()
                .await
                .context("Fetching OpenAI Codex model catalog")?
                .error_for_status()
                .context("OpenAI Codex model catalog request failed")?
                .json::<Catalog>()
                .await
                .context("Invalid OpenAI Codex model catalog")?;
            let mut models = catalog
                .models
                .into_iter()
                .map(|entry| Model {
                    id: entry.slug,
                    name: entry.display_name,
                    supported_parameters: vec!["tools".into(), "reasoning".into()],
                    reasoning: Some(ReasoningCapabilities {
                        mandatory: true,
                        default_effort: None,
                        supported_efforts: SupportedEfforts::Levels(
                            entry
                                .supported_reasoning_levels
                                .into_iter()
                                .map(|level| level.effort)
                                .collect(),
                        ),
                    }),
                })
                .collect::<Vec<_>>();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            models.dedup_by(|a, b| a.id == b.id);
            if models.is_empty() {
                bail!("OpenAI Codex catalog contains no models");
            }
            Ok(models)
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
