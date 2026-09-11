//! `OpenAI Codex` provider using `ChatGPT` OAuth credentials.
use crate::{
    credentials::CodexCredentials,
    providers::{
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
    contexts: std::sync::RwLock<std::collections::HashMap<String, usize>>,
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
            contexts: std::sync::RwLock::default(),
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
    #[serde(default)]
    context_window: Option<usize>,
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
                context_length: entry.context_window,
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
    fn context_window(&self, model: &str) -> Option<usize> {
        self.contexts.read().ok()?.get(model).copied()
    }
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
            let models = catalog_models(catalog)?;
            *self
                .contexts
                .write()
                .map_err(|_| anyhow::anyhow!("Model context cache poisoned"))? = models
                .iter()
                .filter_map(|model| Some((model.id.clone(), model.context_length?)))
                .collect();
            Ok(models)
        })
    }

    fn complete<'a>(&'a self, request: CompletionRequest<'a>) -> ProviderFuture<'a, Completion> {
        Box::pin(async move {
            self.completion(request, None)
                .await
                .map_err(super::ProviderError::normalize)
        })
    }

    fn complete_stream<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        output: &'a crate::events::Output,
    ) -> ProviderFuture<'a, Completion> {
        Box::pin(async move {
            self.completion(request, Some(output))
                .await
                .map_err(super::ProviderError::normalize)
        })
    }
}

impl Codex {
    fn completion<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        output: Option<&'a crate::events::Output>,
    ) -> ProviderFuture<'a, Completion> {
        Box::pin(async move {
            let input = request.messages.iter().flat_map(|message| match message {
                crate::providers::ChatMessage::User { content } => vec![json!({"type":"message","role":"user","content":[{"type":"input_text","text":content}]})],
                crate::providers::ChatMessage::Assistant { content, tool_calls, .. } => {
                    let mut items = content.as_ref().map_or_else(Vec::new, |text| vec![json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})]);
                    items.extend(tool_calls.iter().map(|call| json!({"type":"function_call","call_id":call.id,"name":call.function.name,"arguments":call.function.arguments.as_deref().unwrap_or("{}") }))); items
                }
                crate::providers::ChatMessage::Tool { content, tool_call_id } => vec![json!({"type":"function_call_output","call_id":tool_call_id,"output":content})],
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
            let response = self
                .request(self.client.post(format!("{}/responses", self.base_url)))
                .json(&body)
                .send()
                .await
                .context("Requesting OpenAI Codex completion")?;
            if !response.status().is_success() {
                return Err(super::ProviderError::http(&response).into());
            }
            let mut part = None;
            super::sse::read(response, |data| stream_event(data, output, &mut part)).await
        })
    }
}

fn stream_event(
    data: &str,
    output: Option<&crate::events::Output>,
    part: &mut Option<(u64, u64)>,
) -> Result<Option<Completion>> {
    let event: Value = serde_json::from_str(data).context("Invalid OpenAI Codex stream event")?;
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_text.delta" | "response.refusal.delta") => {
            let text = event
                .get("delta")
                .and_then(Value::as_str)
                .context("Codex text delta is missing text")?;
            if let Some(output) = output {
                let current = (
                    event
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    event
                        .get("content_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                if part.is_some_and(|previous| previous != current) {
                    output.emit(crate::events::Event::AssistantDelta("\n".into()));
                }
                *part = Some(current);
                output.emit(crate::events::Event::AssistantDelta(text.to_owned()));
            }
            Ok(None)
        }
        Some("response.completed") => {
            let response = event
                .get("response")
                .context("Codex completion is missing its response")?;
            if response.get("status").and_then(Value::as_str) != Some("completed") {
                bail!("Codex response did not complete successfully");
            }
            let completion = parse_response(response)?;
            if part.is_some() && completion.content.is_none() {
                bail!("Codex completion is missing the streamed text");
            }
            Ok(Some(completion))
        }
        Some("response.incomplete") => {
            let response = event
                .get("response")
                .context("Incomplete event is missing its response")?;
            let reason = match response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            {
                Some("max_output_tokens") => super::FinishReason::Length,
                Some("content_filter") => super::FinishReason::ContentFilter,
                _ => super::FinishReason::Incomplete,
            };
            Ok(Some(Completion {
                content: None,
                reasoning: None,
                reasoning_details: None,
                tool_calls: Vec::new(),
                usage: response
                    .get("usage")
                    .filter(|usage| !usage.is_null())
                    .map(|usage| serde_json::from_value(usage.clone()))
                    .transpose()?,
                finish_reason: reason,
            }))
        }
        Some("error" | "response.failed") => Err(super::ProviderError::stream(
            event
                .pointer("/response/error")
                .or_else(|| event.get("error"))
                .unwrap_or(&event),
        )
        .into()),
        _ => Ok(None),
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
                            .filter_map(|part| {
                                part.get("text")
                                    .or_else(|| part.get("refusal"))
                                    .and_then(Value::as_str)
                            })
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
        usage: value
            .get("usage")
            .filter(|usage| !usage.is_null())
            .map(|usage| serde_json::from_value(usage.clone()))
            .transpose()?,
        finish_reason: if tool_calls.is_empty() {
            super::FinishReason::Stop
        } else {
            super::FinishReason::ToolCalls
        },
        content: (!text.is_empty()).then(|| text.join("\n")),
        reasoning: (!reasoning.is_empty()).then(|| reasoning.join("\n")),
        reasoning_details: None,
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ReasoningEffort;

    #[test]
    fn incomplete_response_reports_usage_and_output_limit() -> Result<()> {
        let event = json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":20,"output_tokens":10}}});
        let result = stream_event(&event.to_string(), None, &mut None)?.context("completion")?;
        assert_eq!(result.finish_reason, super::super::FinishReason::Length);
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.usage.context("usage")?.output_tokens, 10);
        Ok(())
    }

    #[test]
    fn streams_text_parts_and_returns_only_authoritative_completed_tools() -> Result<()> {
        use crate::events::{Event, Output};
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let output = Output::channel(sender);
        let mut part = None;
        for (index, text) in [(0, "Hello"), (0, " world"), (1, "Next")] {
            assert!(stream_event(&json!({"type":"response.output_text.delta", "output_index":index, "content_index":0, "delta":text}).to_string(), Some(&output), &mut part)?.is_none());
        }
        let mut text = String::new();
        while let Ok(Event::AssistantDelta(delta)) = events.try_recv() {
            text.push_str(&delta);
        }
        assert_eq!(text, "Hello world\nNext");
        let response = json!({"type":"response.completed", "response":{"status":"completed", "output":[
            {"type":"message","content":[{"type":"output_text","text":"Hello world"}]},
            {"type":"message","content":[{"type":"output_text","text":"Next"}]},
            {"type":"function_call","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"x\"}"}
        ]}});
        let complete = stream_event(&response.to_string(), Some(&output), &mut part)?
            .context("Missing completion")?;
        assert_eq!(complete.content.as_deref(), Some(text.as_str()));
        assert_eq!(complete.tool_calls.len(), 1);
        assert!(events.try_recv().is_err());
        for data in [
            "invalid",
            "[DONE]",
            r#"{"type":"response.failed"}"#,
            r#"{"type":"response.incomplete"}"#,
            r#"{"type":"error"}"#,
            r#"{"type":"response.completed","response":{"status":"incomplete","output":[]}}"#,
        ] {
            assert!(stream_event(data, None, &mut part).is_err());
        }
        Ok(())
    }

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
            "../../tests/fixtures/codex_models.json"
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
