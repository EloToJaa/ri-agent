use crate::{
    config::{LuaConfig, ReasoningEffort},
    events::{Event, Output},
    limits::Limits,
    message::Message,
    provider::{CompletionRequest, Provider},
    response_processor::{ResponseProcessor, TurnOutcome},
    sessions::{SavedSession, SessionStore},
    tools,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroUsize, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
}

pub struct AgentConfig {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_turns: NonZeroUsize,
    pub limits: Limits,
    pub lua: Arc<LuaConfig>,
}

/// A conversation shared by the CLI and TUI frontends.
pub struct Session {
    provider: Arc<dyn Provider>,
    config: AgentConfig,
    messages: Vec<Message>,
    output: Output,
    id: String,
    store: Option<SessionStore>,
    revision: i64,
    skills: crate::skills::Skills,
}

impl Session {
    pub fn new(provider: Arc<dyn Provider>, config: AgentConfig, output: Output) -> Self {
        Self {
            provider,
            config,
            messages: Vec::new(),
            output,
            id: uuid::Uuid::new_v4().to_string(),
            store: None,
            revision: 0,
            skills: crate::skills::Skills::default(),
        }
    }

    #[must_use]
    pub fn with_skills(mut self, skills: crate::skills::Skills) -> Self {
        self.skills = skills;
        self
    }

    pub const fn skills(&self) -> &crate::skills::Skills {
        &self.skills
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    #[must_use]
    pub fn with_store(mut self, store: SessionStore) -> Self {
        self.store = Some(store);
        self
    }
    pub fn set_output(&mut self, output: Output) {
        self.output = output;
    }
    pub const fn store(&self) -> Option<&SessionStore> {
        self.store.as_ref()
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn selection(&self) -> Selection {
        Selection {
            model: self.config.model.clone(),
            reasoning_effort: self.config.reasoning_effort,
        }
    }

    pub async fn select(&mut self, selection: Selection) -> Result<()> {
        let previous_selection = self.selection();
        let previous_messages = self.messages.clone();
        if self.config.model != selection.model {
            // Provider-specific signatures/encrypted reasoning must not cross model boundaries.
            for message in &mut self.messages {
                if let Message::Assistant {
                    reasoning,
                    reasoning_details,
                    ..
                } = message
                {
                    *reasoning = None;
                    *reasoning_details = None;
                }
            }
        }
        self.config.model = selection.model;
        self.config.reasoning_effort = selection.reasoning_effort;
        if let Err(error) = self.checkpoint(false, self.messages.len()).await {
            self.config.model = previous_selection.model;
            self.config.reasoning_effort = previous_selection.reasoning_effort;
            self.messages = previous_messages;
            return Err(error);
        }
        Ok(())
    }

    /// Save the current conversation, then start a fresh provider-specific session.
    pub async fn switch_provider(
        &mut self,
        provider: Arc<dyn Provider>,
        model: String,
    ) -> Result<()> {
        if provider.id() == self.provider.id() {
            return Ok(());
        }
        self.checkpoint(false, self.messages.len()).await?;
        self.clear();
        self.provider = provider;
        self.config.model = model;
        self.config.reasoning_effort = None;
        Ok(())
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.id = uuid::Uuid::new_v4().to_string();
        self.revision = 0;
    }

    /// Restores a checkpoint, never executing tools. Returns true for an interrupted run.
    pub async fn resume(&mut self, id: &str) -> Result<bool> {
        let store = self
            .store
            .as_ref()
            .context("Session persistence is disabled")?;
        let mut saved = store.load(id.to_owned()).await?;
        if saved.provider != self.provider.id() {
            bail!(
                "Session belongs to provider '{}', not '{}'",
                saved.provider,
                self.provider.id()
            );
        }
        if saved.interrupted {
            saved.messages.truncate(saved.stable_len);
        }
        self.id = saved.id;
        self.messages = saved.messages;
        self.config.model = saved.selection.model;
        self.config.reasoning_effort = saved.selection.reasoning_effort;
        self.revision = saved.revision;
        Ok(saved.interrupted)
    }

    pub fn history(&self) -> Vec<Event> {
        self.messages
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => Some(Event::User(content.clone())),
                Message::Assistant {
                    content: Some(content),
                    ..
                } => Some(Event::Assistant(content.clone())),
                Message::Tool {
                    tool_call_id,
                    content,
                } => Some(Event::Tool(format!("{tool_call_id}:\n{content}"))),
                Message::Assistant { .. } => None,
            })
            .collect()
    }

    async fn checkpoint(&mut self, interrupted: bool, stable_len: usize) -> Result<()> {
        // Selecting options in a fresh composer must not create an empty session.
        if self.messages.is_empty() && self.revision == 0 {
            return Ok(());
        }
        let Some(store) = &self.store else {
            return Ok(());
        };
        self.revision = store
            .save(SavedSession {
                id: self.id.clone(),
                provider: self.provider.id().to_owned(),
                selection: self.selection(),
                interrupted,
                messages: self.messages.clone(),
                stable_len,
                revision: self.revision,
            })
            .await
            .context("Saving session checkpoint")?;
        Ok(())
    }

    /// Failed turns are removed from history; local tool side effects are not undone.
    pub async fn submit(&mut self, prompt: String) -> Result<()> {
        let length = self.messages.len();
        let result = self.run_turn(prompt, length).await;
        if result.is_err() {
            self.messages.truncate(length);
        }
        self.checkpoint(false, self.messages.len()).await?;
        result
    }

    async fn run_turn(&mut self, prompt: String, stable_len: usize) -> Result<()> {
        let prompt = self.skills.expand(prompt).await?;
        let prompt = self.config.lua.hook("before_prompt", prompt).await?;
        self.messages.push(Message::User { content: prompt });
        self.checkpoint(true, stable_len).await?;
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
            let response = self
                .provider
                .complete(CompletionRequest {
                    messages: &self.messages,
                    model: &self.config.model,
                    tools: &definitions,
                    reasoning_effort: self.config.reasoning_effort,
                })
                .await
                .with_context(|| {
                    format!("Failed to request {} model response", self.provider.name())
                })?;
            let outcome = ResponseProcessor::new(response)
                .with_limits(self.config.limits)
                .with_runtime(self.output.clone(), Arc::clone(&self.config.lua))
                .process(&mut self.messages)
                .await?;
            if outcome == TurnOutcome::Finished {
                return Ok(());
            }
            self.checkpoint(true, stable_len).await?;
        }
        bail!("Maximum model turns ({}) reached", self.config.max_turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Completion, Model, ProviderFuture};

    struct MockProvider(&'static str);
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            self.0
        }
        fn models(&self) -> ProviderFuture<'_, Vec<Model>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn complete<'a>(&'a self, _: CompletionRequest<'a>) -> ProviderFuture<'a, Completion> {
            Box::pin(async { bail!("Unexpected completion") })
        }
    }

    #[tokio::test]
    async fn provider_switch_preserves_saved_history_and_resets_selection() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ri-provider-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root)?;
        let store = SessionStore::open(root.join("sessions.sqlite3"), &root)?;
        let mut session = Session::new(
            Arc::new(MockProvider("openrouter")),
            AgentConfig {
                model: "old-model".into(),
                reasoning_effort: Some(ReasoningEffort::High),
                max_turns: NonZeroUsize::MIN,
                limits: Limits::default(),
                lua: LuaConfig::from_source("return {}", "test")?,
            },
            Output::default(),
        )
        .with_store(store.clone());
        session.messages.push(Message::User {
            content: "old prompt".into(),
        });
        let old_id = session.id().to_owned();
        session
            .switch_provider(Arc::new(MockProvider("openrouter")), "ignored".into())
            .await?;
        assert_eq!(session.id(), old_id);
        assert_eq!(session.selection().model, "old-model");
        session
            .switch_provider(Arc::new(MockProvider("openai-codex")), "codex-model".into())
            .await?;
        assert_ne!(session.id(), old_id);
        assert_eq!(session.provider().id(), "openai-codex");
        assert!(session.history().is_empty());
        assert_eq!(
            session.selection(),
            Selection {
                model: "codex-model".into(),
                reasoning_effort: None
            }
        );
        let saved = store.load(old_id.clone()).await?;
        assert_eq!(saved.provider, "openrouter");
        assert_eq!(saved.messages.len(), 1);
        assert!(session.resume(&old_id).await.is_err());
        session
            .switch_provider(Arc::new(MockProvider("openrouter")), "default".into())
            .await?;
        session.resume(&old_id).await?;
        assert_eq!(session.history().len(), 1);
        assert_eq!(
            session.selection().reasoning_effort,
            Some(ReasoningEffort::High)
        );
        // A concurrent writer must prevent switching rather than losing this session.
        store.save(store.load(old_id.clone()).await?).await?;
        assert!(
            session
                .switch_provider(Arc::new(MockProvider("openai-codex")), "new".into())
                .await
                .is_err()
        );
        assert_eq!(session.id(), old_id);
        assert_eq!(session.provider().id(), "openrouter");
        assert_eq!(session.history().len(), 1);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
