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
            Ok(models)
        })
    }

    fn complete<'a>(&'a self, request: CompletionRequest<'a>) -> ProviderFuture<'a, Completion> {
        Box::pin(async move {
            #[derive(Serialize)]
            struct Reasoning {
                effort: crate::config::ReasoningEffort,
            }
            #[derive(Serialize)]
            struct ChatRequest<'a> {
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
