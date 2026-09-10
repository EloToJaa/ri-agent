use crate::{
    config::{LuaConfig, ReasoningEffort},
    events::{Event, Output},
    limits::Limits,
    message::Message,
    providers::{CompletionRequest, Provider},
    response_processor::{ResponseProcessor, TurnOutcome},
    sessions::{SavedSession, SessionStore},
    tools,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroUsize, sync::Arc};

const CANCELLED_NOTICE: &str = "[Harness notice: the user cancelled this turn. Completed tool effects remain; tools marked executed=false were skipped. Follow the user's next instructions.]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextStats {
    pub messages: usize,
    pub approximate_tokens: usize,
}

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

    pub fn context_stats(&self) -> ContextStats {
        ContextStats {
            messages: self.messages.len(),
            approximate_tokens: self
                .messages
                .iter()
                .map(|message| {
                    serde_json::to_string(message).map_or(0, |text| text.len().div_ceil(4))
                })
                .sum(),
        }
    }

    pub async fn compact(&mut self) -> Result<ContextStats> {
        const KEEP: usize = 6;
        if self.messages.len() <= KEEP {
            return Ok(self.context_stats());
        }
        let split = self.messages.len() - KEEP;
        let summary = self
            .messages
            .get(..split)
            .context("Invalid compaction boundary")?
            .iter()
            .filter_map(|message| match message {
                Message::User { content } => Some(format!(
                    "User: {}",
                    content.chars().take(240).collect::<String>()
                )),
                Message::Assistant {
                    content: Some(content),
                    ..
                } => Some(format!(
                    "Assistant: {}",
                    content.chars().take(240).collect::<String>()
                )),
                Message::Tool {
                    tool_call_id,
                    content,
                } => Some(format!(
                    "Tool {tool_call_id}: {}",
                    content.chars().take(160).collect::<String>()
                )),
                Message::Assistant { content: None, .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.messages.drain(..split);
        self.messages.insert(
            0,
            Message::User {
                content: format!("[Conversation summary; older messages compacted]\n{summary}"),
            },
        );
        self.checkpoint(false, self.messages.len()).await?;
        Ok(self.context_stats())
    }

    /// Save the current checkpoint and continue from it under a new session ID.
    pub async fn fork(&mut self) -> Result<String> {
        self.checkpoint(false, self.messages.len()).await?;
        let parent = self.id.clone();
        self.id = uuid::Uuid::new_v4().to_string();
        self.revision = 0;
        self.checkpoint(false, self.messages.len()).await?;
        Ok(parent)
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
                Message::User { content } if content == CANCELLED_NOTICE => {
                    Some(Event::Progress(content.clone()))
                }
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
        self.submit_cancellable(prompt, &crate::cancellation::Cancellation::default())
            .await
            .map(|_| ())
    }

    /// Cancel model requests immediately, but finish active tools/hooks before saving.
    /// Successful tool results and explicit skipped results survive orderly cancellation.
    pub async fn submit_cancellable(
        &mut self,
        prompt: String,
        cancellation: &crate::cancellation::Cancellation,
    ) -> Result<SubmitOutcome> {
        let length = self.messages.len();
        let result = self.run_turn(prompt, length, cancellation).await;
        if result.is_err() {
            self.output.emit(Event::AssistantAborted);
            self.messages.truncate(length);
        }
        if matches!(result, Ok(SubmitOutcome::Cancelled)) {
            self.output.emit(Event::AssistantAborted);
            if self.messages.len() > length {
                self.messages.push(Message::User {
                    content: CANCELLED_NOTICE.into(),
                });
            }
        }
        self.checkpoint(false, self.messages.len()).await?;
        if matches!(result, Ok(SubmitOutcome::Cancelled)) {
            self.output.emit(Event::Progress("Turn cancelled. Completed tool effects and results were retained; pending tools were skipped.".into()));
        }
        result
    }

    async fn run_turn(
        &mut self,
        prompt: String,
        stable_len: usize,
        cancellation: &crate::cancellation::Cancellation,
    ) -> Result<SubmitOutcome> {
        if cancellation.is_cancelled() {
            return Ok(SubmitOutcome::Cancelled);
        }
        let prompt = self.skills.expand(prompt).await?;
        let prompt = self.config.lua.hook("before_prompt", prompt).await?;
        if cancellation.is_cancelled() {
            return Ok(SubmitOutcome::Cancelled);
        }
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
            let request = CompletionRequest {
                messages: &self.messages,
                model: &self.config.model,
                tools: &definitions,
                reasoning_effort: self.config.reasoning_effort,
            };
            // Hooks transform complete text. Do not display untransformed deltas.
            let response = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(SubmitOutcome::Cancelled),
                response = async {
                    if self.config.lua.has_after_response_hook {
                        self.provider.complete(request).await
                    } else {
                        self.provider.complete_stream(request, &self.output).await
                    }
                } => response,
            }
            .with_context(|| {
                format!("Failed to request {} model response", self.provider.name())
            })?;
            let outcome = ResponseProcessor::new(response)
                .with_cancellation(cancellation.clone())
                .with_limits(self.config.limits)
                .with_runtime(self.output.clone(), Arc::clone(&self.config.lua))
                .process(&mut self.messages)
                .await?;
            if outcome == TurnOutcome::Finished {
                return Ok(SubmitOutcome::Completed);
            }
            if outcome == TurnOutcome::Cancelled {
                return Ok(SubmitOutcome::Cancelled);
            }
            self.checkpoint(true, stable_len).await?;
            if cancellation.is_cancelled() {
                return Ok(SubmitOutcome::Cancelled);
            }
        }
        bail!("Maximum model turns ({}) reached", self.config.max_turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Completion, Model, ProviderFuture};

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

    struct CancellationProvider {
        requests: std::sync::Mutex<Vec<Vec<Message>>>,
        first: serde_json::Value,
        pending: bool,
    }

    impl Provider for CancellationProvider {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn name(&self) -> &'static str {
            "Mock"
        }
        fn models(&self) -> ProviderFuture<'_, Vec<Model>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn complete<'a>(
            &'a self,
            request: CompletionRequest<'a>,
        ) -> ProviderFuture<'a, Completion> {
            Box::pin(async move {
                let mut requests = self
                    .requests
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Poisoned requests"))?;
                requests.push(request.messages.to_vec());
                let response = if requests.len() == 1 {
                    self.first.clone()
                } else {
                    serde_json::json!({"content":"Corrected"})
                };
                drop(requests);
                Ok(serde_json::from_value(response)?)
            })
        }
        fn complete_stream<'a>(
            &'a self,
            request: CompletionRequest<'a>,
            output: &'a Output,
        ) -> ProviderFuture<'a, Completion> {
            Box::pin(async move {
                if self.pending {
                    output.emit(Event::AssistantDelta("partial text".into()));
                    return std::future::pending().await;
                }
                self.complete(request).await
            })
        }
    }

    fn cancellation_session(provider: Arc<dyn Provider>, output: Output) -> Result<Session> {
        Ok(Session::new(
            provider,
            AgentConfig {
                model: "mock".into(),
                reasoning_effort: None,
                max_turns: NonZeroUsize::MIN.saturating_add(2),
                limits: Limits::default(),
                lua: LuaConfig::from_source("return {}", "test")?,
            },
            output,
        ))
    }

    #[tokio::test]
    async fn cancels_a_pending_model_request_without_committing_partial_text() -> Result<()> {
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let provider = Arc::new(CancellationProvider {
            requests: std::sync::Mutex::default(),
            first: serde_json::Value::Null,
            pending: true,
        });
        let mut session = cancellation_session(provider, Output::channel(sender))?;
        let cancellation = crate::cancellation::Cancellation::default();
        let cancel = async {
            while let Some(event) = events.recv().await {
                if matches!(event, Event::AssistantDelta(_)) {
                    cancellation.cancel();
                    return;
                }
            }
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                session.submit_cancellable("start".into(), &cancellation),
                cancel
            )
        })
        .await?;
        assert_eq!(result?, SubmitOutcome::Cancelled);
        assert_eq!(session.messages.len(), 2);
        assert!(
            !session
                .messages
                .iter()
                .any(|message| matches!(message, Message::Assistant { .. }))
        );
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, Event::AssistantAborted))
        );
        let fresh = crate::cancellation::Cancellation::default();
        assert!(!fresh.is_cancelled());
        let length = session.messages.len();
        assert_eq!(
            session
                .submit_cancellable("already cancelled".into(), &cancellation)
                .await?,
            SubmitOutcome::Cancelled
        );
        assert_eq!(session.messages.len(), length);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_finishes_active_tools_skips_pending_tools_and_resumes_without_replay()
    -> Result<()> {
        use serde_json::json;
        let directory = tempfile::tempdir()?;
        let kept = directory.path().join("kept");
        let skipped = directory.path().join("skipped");
        let call = |id: &str, name: &str, args: serde_json::Value| json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}});
        let provider = Arc::new(CancellationProvider {
            requests: std::sync::Mutex::default(),
            pending: false,
            first: json!({"tool_calls":[
                call("kept", "Write", json!({"file_path":kept,"content":"retained"})),
                call("active", "Bash", json!({"command":"exec sleep 0.05"})),
                call("skipped", "Write", json!({"file_path":skipped,"content":"must not exist"}))
            ]}),
        });
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let store =
            SessionStore::open(directory.path().join("sessions.sqlite3"), directory.path())?;
        let mut session = cancellation_session(provider.clone(), Output::channel(sender))?
            .with_store(store.clone());
        let cancellation = crate::cancellation::Cancellation::default();
        let cancel = async {
            while let Some(event) = events.recv().await {
                if matches!(event, Event::Progress(text) if text.starts_with("Running Bash")) {
                    cancellation.cancel();
                    return;
                }
            }
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(
                session.submit_cancellable("start".into(), &cancellation),
                cancel
            )
        })
        .await?;
        assert_eq!(result?, SubmitOutcome::Cancelled);
        assert_eq!(tokio::fs::read_to_string(&kept).await?, "retained");
        assert!(!skipped.exists());
        let saved = store.load(session.id().to_owned()).await?;
        assert!(!saved.interrupted);
        assert_eq!(saved.messages.len(), 6);
        let results: Vec<_> = saved
            .messages
            .iter()
            .filter_map(|message| {
                if let Message::Tool {
                    tool_call_id,
                    content,
                } = message
                {
                    Some((tool_call_id.as_str(), content.as_str()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(results.first().map(|result| result.0), Some("kept"));
        let active: serde_json::Value =
            serde_json::from_str(results.get(1).context("Missing active tool result")?.1)?;
        assert_eq!(active.get("success"), Some(&json!(true)));
        let skipped_result: serde_json::Value =
            serde_json::from_str(results.get(2).context("Missing skipped result")?.1)?;
        assert_eq!(skipped_result.get("executed"), Some(&json!(false)));
        tokio::fs::remove_file(&kept).await?;
        let id = session.id().to_owned();
        assert!(!session.resume(&id).await?);
        assert!(!kept.exists());
        session.submit("correction".into()).await?;
        assert!(!kept.exists());
        let requests = provider
            .requests
            .lock()
            .map_err(|_| anyhow::anyhow!("Poisoned requests"))?;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests.get(1).context("Missing correction")?.len(), 7);
        drop(requests);
        Ok(())
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

    #[tokio::test]
    async fn compacts_old_messages_and_reports_bounded_context() -> Result<()> {
        let mut session = Session::new(
            Arc::new(MockProvider("openrouter")),
            AgentConfig {
                model: "mock".into(),
                reasoning_effort: None,
                max_turns: NonZeroUsize::MIN,
                limits: Limits::default(),
                lua: LuaConfig::from_source("return {}", "test")?,
            },
            Output::default(),
        );
        for index in 0..10 {
            session.messages.push(Message::User {
                content: format!("message {index}"),
            });
        }
        let before = session.context_stats();
        let after = session.compact().await?;
        assert_eq!(before.messages, 10);
        assert_eq!(after.messages, 7);
        assert!(
            matches!(session.messages.first(), Some(Message::User { content }) if content.contains("message 0"))
        );
        assert!(
            matches!(session.messages.last(), Some(Message::User { content }) if content == "message 9")
        );
        Ok(())
    }

    #[tokio::test]
    async fn forks_saved_history_under_a_new_id() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = SessionStore::open(root.path().join("sessions.sqlite3"), root.path())?;
        let mut session = Session::new(
            Arc::new(MockProvider("openrouter")),
            AgentConfig {
                model: "mock".into(),
                reasoning_effort: None,
                max_turns: NonZeroUsize::MIN,
                limits: Limits::default(),
                lua: LuaConfig::from_source("return {}", "test")?,
            },
            Output::default(),
        )
        .with_store(store.clone());
        session.messages.push(Message::User {
            content: "shared context".into(),
        });
        let original = session.id().to_owned();
        let parent = session.fork().await?;
        assert_eq!(parent, original);
        assert_ne!(session.id(), original);
        assert_eq!(session.history().len(), 1);
        assert_eq!(store.list().await?.len(), 2);
        assert_eq!(store.load(original).await?.messages.len(), 1);
        assert_eq!(store.load(session.id().to_owned()).await?.messages.len(), 1);
        Ok(())
    }
}
