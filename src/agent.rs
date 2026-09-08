use crate::{
    limits::Limits,
    message::Message,
    response::Response,
    response_processor::{ResponseProcessor, TurnOutcome},
    tools,
};
use anyhow::{Context, Result, bail};
use async_openai::{Client, config::OpenAIConfig};
use serde::Serialize;
use std::num::NonZeroUsize;

pub(crate) struct AgentConfig {
    pub(crate) model: String,
    pub(crate) max_turns: NonZeroUsize,
    pub(crate) limits: Limits,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    messages: &'a [Message],
    model: &'a str,
    tools: &'a [tools::ToolDefinition],
}

pub(crate) async fn run(
    client: &Client<OpenAIConfig>,
    config: &AgentConfig,
    prompt: String,
) -> Result<()> {
    let mut messages = vec![Message::User { content: prompt }];
    let definitions = tools::definitions();

    for turn in 0..config.max_turns.get() {
        eprintln!(
            "Requesting model response ({}/{})...",
            turn + 1,
            config.max_turns
        );
        let response: Response = client
            .chat()
            .create_byot(ChatRequest {
                messages: &messages,
                model: &config.model,
                tools: &definitions,
            })
            .await
            .context("Failed to request model response")?;

        if let TurnOutcome::Finished = ResponseProcessor::new(response)
            .with_limits(config.limits)
            .process(&mut messages)
            .await?
        {
            return Ok(());
        }
    }

    bail!("Maximum model turns ({}) reached", config.max_turns)
}
