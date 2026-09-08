use crate::{
    config::LuaConfig,
    events::{Event, Output},
    limits::Limits,
    message::Message,
    response::Response,
    response_processor::{ResponseProcessor, TurnOutcome},
    tools,
};
use anyhow::{Context, Result, bail};
use async_openai::{Client, config::OpenAIConfig};
use serde::Serialize;
use std::{num::NonZeroUsize, sync::Arc};

pub struct AgentConfig {
    pub model: String,
    pub max_turns: NonZeroUsize,
    pub limits: Limits,
    pub lua: Arc<LuaConfig>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    messages: &'a [Message],
    model: &'a str,
    tools: &'a [serde_json::Value],
}

/// A conversation shared by the CLI and TUI frontends.
pub struct Session {
    client: Client<OpenAIConfig>,
    config: AgentConfig,
    messages: Vec<Message>,
    output: Output,
}

impl Session {
    pub fn new(base_url: String, api_key: String, config: AgentConfig, output: Output) -> Self {
        let client = Client::with_config(
            OpenAIConfig::new()
                .with_api_base(base_url)
                .with_api_key(api_key),
        );
        Self {
            client,
            config,
            messages: Vec::new(),
            output,
        }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Failed turns are removed from history; local tool side effects are not undone.
    pub async fn submit(&mut self, prompt: String) -> Result<()> {
        let length = self.messages.len();
        let result = self.run_turn(prompt).await;
        if result.is_err() {
            self.messages.truncate(length);
        }
        result
    }

    async fn run_turn(&mut self, prompt: String) -> Result<()> {
        let prompt = self.config.lua.hook("before_prompt", prompt).await?;
        self.messages.push(Message::User { content: prompt });
        let mut definitions = tools::definitions()
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        definitions.extend(self.config.lua.definitions.iter().cloned());

        for turn in 0..self.config.max_turns.get() {
            self.output.emit(Event::Progress(format!(
                "Requesting model response ({}/{})...",
                turn + 1,
                self.config.max_turns
            )));
            let response: Response = self
                .client
                .chat()
                .create_byot(ChatRequest {
                    messages: &self.messages,
                    model: &self.config.model,
                    tools: &definitions,
                })
                .await
                .context("Failed to request model response")?;

            if let TurnOutcome::Finished = ResponseProcessor::new(response)
                .with_limits(self.config.limits)
                .with_runtime(self.output.clone(), Arc::clone(&self.config.lua))
                .process(&mut self.messages)
                .await?
            {
                return Ok(());
            }
        }
        bail!("Maximum model turns ({}) reached", self.config.max_turns)
    }
}
