//! `OpenRouter` implementation of the provider boundary and authentication API.
pub use crate::config::ReasoningEffort;
pub use crate::providers::{Model, ReasoningCapabilities, SupportedEfforts};
use crate::{
    credentials::ApiKey,
    providers::{Completion, CompletionRequest, Provider, ProviderFuture},
    response::Response,
};
use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
struct KeyInfo {
    data: serde_json::Map<String, serde_json::Value>,
}
#[derive(Deserialize)]
struct Exchange {
    key: String,
}
#[derive(Deserialize)]
struct Catalog {
    data: Vec<Model>,
}

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const AUTH_URL: &str = "https://openrouter.ai/auth";

pub fn models(base_url: &str) -> ProviderFuture<'static, Vec<Model>> {
    let base_url = base_url.to_owned();
    Box::pin(async move {
        OpenRouter::new(&base_url, crate::credentials::api_key("catalog")?)?
            .models()
            .await
    })
}

pub struct OpenRouter {
    client: Client,
    base_url: String,
    key: ApiKey,
    contexts: std::sync::RwLock<std::collections::HashMap<String, usize>>,
}

impl OpenRouter {
    pub fn new(base_url: &str, key: ApiKey) -> Result<Self> {
        let url = Url::parse(base_url).context("Invalid OpenRouter API URL")?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            bail!("OpenRouter API URL must use HTTPS (HTTP is allowed only for localhost tests)");
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("OpenRouter API URL cannot contain credentials, a query, or a fragment");
        }
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_mins(2))
                .build()?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            key,
            contexts: std::sync::RwLock::default(),
        })
    }

    /// Validate a key without exposing the upstream response or key in errors.
    pub async fn validate_key(&self) -> Result<()> {
        let response = self
            .client
            .get(format!("{}/key", self.base_url))
            .bearer_auth(self.key.expose_secret())
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("Could not reach OpenRouter to validate the API key")?;
        if !response.status().is_success() {
            bail!(
                "OpenRouter rejected API key validation (HTTP {})",
                response.status()
            );
        }
        let info = response
            .json::<KeyInfo>()
            .await
            .map_err(|_| anyhow::anyhow!("Invalid OpenRouter key validation response"))?;
        if info.data.is_empty() {
            bail!("OpenRouter key validation returned no key metadata");
        }
        Ok(())
    }

    /// Exchange a single-use PKCE authorization code. Used only against the official API by the CLI.
    pub async fn exchange_code(&self, code: &str, verifier: &str) -> Result<ApiKey> {
        let response = self.client.post(format!("{}/auth/keys", self.base_url))
            .timeout(Duration::from_secs(20))
            .json(&serde_json::json!({"code":code,"code_verifier":verifier,"code_challenge_method":"S256"}))
            .send().await.context("Could not reach OpenRouter to exchange the authorization code")?;
        if !response.status().is_success() {
            bail!(
                "OpenRouter authorization exchange failed (HTTP {}); run 'ri login' again",
                response.status()
            );
        }
        let exchange = response
            .json::<Exchange>()
            .await
            .map_err(|_| anyhow::anyhow!("Invalid OpenRouter authorization response"))?;
        crate::credentials::api_key(exchange.key)
    }
}

impl Provider for OpenRouter {
    fn context_window(&self, model: &str) -> Option<usize> {
        self.contexts.read().ok()?.get(model).copied()
    }
    fn id(&self) -> &'static str {
        "openrouter"
    }
    fn name(&self) -> &'static str {
        "OpenRouter"
    }

    fn models(&self) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async {
            // The public catalog request intentionally carries no credentials.
            let mut models = self
                .client
                .get(format!("{}/models", self.base_url))
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .context("Fetching OpenRouter model catalog")?
                .error_for_status()
                .context("OpenRouter model catalog request failed")?
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
        self.completion(request, None)
    }

    fn complete_stream<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        output: &'a crate::events::Output,
    ) -> ProviderFuture<'a, Completion> {
        self.completion(request, Some(output))
    }
}

impl OpenRouter {
    fn completion<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        output: Option<&'a crate::events::Output>,
    ) -> ProviderFuture<'a, Completion> {
        Box::pin(async move {
            #[derive(Serialize)]
            struct Reasoning {
                effort: crate::config::ReasoningEffort,
            }
            #[derive(Serialize)]
            struct ChatRequest<'a> {
                stream: bool,
                messages: &'a [crate::providers::ChatMessage],
                model: &'a str,
                tools: &'a [serde_json::Value],
                #[serde(skip_serializing_if = "Option::is_none")]
                reasoning: Option<Reasoning>,
            }
            let response = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(self.key.expose_secret())
                .json(&ChatRequest {
                    stream: output.is_some(),
                    messages: request.messages,
                    model: request.model,
                    tools: request.tools,
                    reasoning: request.reasoning_effort.map(|effort| Reasoning { effort }),
                })
                .send()
                .await
                .context("Requesting OpenRouter completion")?;
            if !response.status().is_success() {
                bail!(
                    "OpenRouter completion failed (HTTP {}); check credentials, model access, and available credits",
                    response.status()
                );
            }
            if response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
                })
            {
                let mut state = StreamCompletion::default();
                return super::sse::read(response, |data| state.push(data, output)).await;
            }
            // Some compatible endpoints ignore stream=true and return JSON.
            let response: Response = response
                .json()
                .await
                .context("Invalid OpenRouter completion response")?;
            response
                .choices
                .into_iter()
                .next()
                .map(|choice| choice.message)
                .context("Response contains no choices")
        })
    }
}

#[derive(Default)]
struct StreamCompletion {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_details: Vec<serde_json::Value>,
    calls: std::collections::BTreeMap<usize, crate::response::ToolCall>,
    finished: bool,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    index: usize,
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    #[serde(alias = "reasoning_content")]
    reasoning: Option<String>,
    reasoning_details: Option<Vec<serde_json::Value>>,
    tool_calls: Option<Vec<CallDelta>>,
}

#[derive(Deserialize)]
struct CallDelta {
    index: usize,
    id: Option<String>,
    r#type: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

fn append(target: &mut Option<String>, delta: Option<String>) {
    if let Some(delta) = delta {
        target.get_or_insert_default().push_str(&delta);
    }
}

impl StreamCompletion {
    fn push(
        &mut self,
        data: &str,
        output: Option<&crate::events::Output>,
    ) -> Result<Option<Completion>> {
        if data == "[DONE]" {
            if !self.finished {
                bail!("OpenRouter stream ended without a finish reason");
            }
            for call in self.calls.values() {
                if call.id.is_empty() || call.function.name.is_empty() || call.r#type != "function"
                {
                    bail!("OpenRouter stream contains an incomplete tool call");
                }
                serde_json::from_str::<serde_json::Value>(
                    call.function
                        .arguments
                        .as_deref()
                        .context("Streamed tool call has no arguments")?,
                )
                .context("Streamed tool call has incomplete JSON arguments")?;
            }
            return Ok(Some(Completion {
                content: self.content.take(),
                reasoning: self.reasoning.take(),
                reasoning_details: (!self.reasoning_details.is_empty())
                    .then(|| std::mem::take(&mut self.reasoning_details)),
                tool_calls: std::mem::take(&mut self.calls).into_values().collect(),
            }));
        }
        let chunk: StreamChunk =
            serde_json::from_str(data).context("Invalid OpenRouter stream event")?;
        if chunk.error.is_some() {
            bail!("OpenRouter reported an error during streaming");
        }
        for choice in chunk.choices.into_iter().filter(|choice| choice.index == 0) {
            if self.finished {
                bail!("OpenRouter sent a choice after its finish reason");
            }
            if let Some(reason) = &choice.finish_reason {
                if !matches!(reason.as_str(), "stop" | "tool_calls") {
                    bail!("OpenRouter response did not finish successfully ({reason})");
                }
                self.finished = true;
            }
            let delta = choice.delta;
            if let (Some(text), Some(output)) = (&delta.content, output) {
                output.emit(crate::events::Event::AssistantDelta(text.clone()));
            }
            append(&mut self.content, delta.content);
            append(&mut self.reasoning, delta.reasoning);
            for detail in delta.reasoning_details.unwrap_or_default() {
                self.merge_reasoning(detail)?;
            }
            for delta in delta.tool_calls.unwrap_or_default() {
                let call =
                    self.calls
                        .entry(delta.index)
                        .or_insert_with(|| crate::response::ToolCall {
                            id: String::new(),
                            r#type: String::new(),
                            function: crate::response::ToolCallFunction {
                                name: String::new(),
                                arguments: None,
                            },
                        });
                if let Some(id) = delta.id {
                    call.id.push_str(&id);
                }
                if let Some(kind) = delta.r#type {
                    call.r#type = kind;
                }
                if let Some(function) = delta.function {
                    if let Some(name) = function.name {
                        call.function.name.push_str(&name);
                    }
                    append(&mut call.function.arguments, function.arguments);
                }
            }
        }
        Ok(None)
    }

    fn merge_reasoning(&mut self, detail: serde_json::Value) -> Result<()> {
        let object = detail
            .as_object()
            .context("Invalid streamed reasoning detail")?;
        let existing = self.reasoning_details.iter_mut().find(|existing| {
            object
                .get("index")
                .filter(|index| !index.is_null())
                .is_some_and(|index| existing.get("index") == Some(index))
                && existing.get("type") == object.get("type")
        });
        let Some(existing) = existing else {
            self.reasoning_details.push(detail);
            return Ok(());
        };
        let existing = existing
            .as_object_mut()
            .context("Invalid accumulated reasoning detail")?;
        for (key, value) in object {
            if value.is_null() {
                continue;
            }
            if matches!(key.as_str(), "text" | "summary" | "data" | "signature")
                && let (Some(serde_json::Value::String(current)), Some(delta)) =
                    (existing.get_mut(key), value.as_str())
            {
                current.push_str(delta);
                continue;
            }
            existing.insert(key.clone(), value.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assembles_interleaved_calls_text_and_signed_reasoning() -> Result<()> {
        let mut state = StreamCompletion::default();
        for delta in [
            json!({"content":"Hello ", "reasoning":"think", "reasoning_details":[{"index":0,"type":"reasoning.text","text":"think", "signature":null}],
                "tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"Read","arguments":"{\"file_"}}]}),
            json!({"content":"世界", "reasoning":"ing", "reasoning_details":[{"index":0,"type":"reasoning.text","text":"ing","signature":"sig"}],
                "tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"Find","arguments":"{\"pattern\":\"\"}"}},
                    {"index":1,"function":{"arguments":"path\":\"x\"}"}}]}),
        ] {
            assert!(
                state
                    .push(
                        &json!({"choices":[{"index":0,"delta":delta}]}).to_string(),
                        None
                    )?
                    .is_none()
            );
        }
        state.push(
            &json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
            None,
        )?;
        state.push(
            &json!({"choices":[],"usage":{"total_tokens":10}}).to_string(),
            None,
        )?;
        let result = state.push("[DONE]", None)?.context("Missing completion")?;
        assert_eq!(result.content.as_deref(), Some("Hello 世界"));
        assert_eq!(result.reasoning.as_deref(), Some("thinking"));
        assert_eq!(
            result.reasoning_details,
            Some(vec![
                json!({"index":0,"type":"reasoning.text","text":"thinking","signature":"sig"})
            ])
        );
        assert_eq!(
            result
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            result
                .tool_calls
                .get(1)
                .context("Missing call")?
                .function
                .arguments
                .as_deref(),
            Some("{\"file_path\":\"x\"}")
        );
        Ok(())
    }

    #[test]
    fn rejects_errors_truncated_completions_and_unfinished_arguments() -> Result<()> {
        for data in [
            "not json",
            "[DONE]",
            r#"{"error":{"message":"upstream failure"}}"#,
            r#"{"choices":[{"index":0,"finish_reason":"length"}]}"#,
        ] {
            assert!(StreamCompletion::default().push(data, None).is_err());
        }
        let mut state = StreamCompletion::default();
        state.push(
            &json!({"choices":[{"index":0,"finish_reason":"tool_calls","delta":{"tool_calls":[{
                "index":0,"id":"x","type":"function","function":{"name":"Write","arguments":"{"}
            }]}}]})
            .to_string(),
            None,
        )?;
        assert!(state.push("[DONE]", None).is_err());
        Ok(())
    }
}
