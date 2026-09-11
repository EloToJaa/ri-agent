use crate::{
    config::{LuaConfig, ReasoningEffort},
    events::{Event, Output},
    limits::Limits,
    message::Message,
    providers::{CompletionRequest, Provider},
    response_processor::{ResponseProcessor, TurnOutcome},
    sessions::{SavedSession, SessionStatus, SessionStore},
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

const MAX_PROVIDER_ATTEMPTS: usize = 3;

#[derive(Debug, thiserror::Error)]
#[error("Maximum model turns ({0}) reached")]
struct TurnLimitReached(usize);

fn retryable_provider_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "connection",
        "429",
        "500",
        "502",
        "503",
        "504",
        "rate limit",
        "temporarily unavailable",
    ]
    .iter()
    .any(|marker| text.contains(marker))
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
    auto_compact_tokens: Option<NonZeroUsize>,
    provider: Arc<dyn Provider>,
    config: AgentConfig,
    messages: Vec<Message>,
    output: Output,
    id: String,
    store: Option<SessionStore>,
    revision: i64,
    skills: crate::skills::Skills,
    instruction_directory: Option<std::path::PathBuf>,
    status: SessionStatus,
}

impl Session {
    pub fn new(provider: Arc<dyn Provider>, config: AgentConfig, output: Output) -> Self {
        Self {
            auto_compact_tokens: NonZeroUsize::new(32_000),
            provider,
            config,
            messages: Vec::new(),
            output,
            id: uuid::Uuid::new_v4().to_string(),
            store: None,
            revision: 0,
            skills: crate::skills::Skills::default(),
            instruction_directory: None,
            status: SessionStatus::Ready,
        }
    }

    /// Override the estimated history token threshold (default 32,000); None disables it.
    #[must_use]
    pub const fn with_auto_compact_tokens(mut self, threshold: Option<NonZeroUsize>) -> Self {
        self.auto_compact_tokens = threshold;
        self
    }

    #[must_use]
    pub fn with_skills(mut self, skills: crate::skills::Skills) -> Self {
        self.skills = skills;
        self
    }

    pub const fn skills(&self) -> &crate::skills::Skills {
        &self.skills
    }

    /// Reload scoped AGENTS.md files from this directory before each submitted prompt.
    #[must_use]
    pub fn with_project_instructions(mut self, directory: std::path::PathBuf) -> Self {
        self.instruction_directory = Some(directory);
        self
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
    pub const fn status(&self) -> SessionStatus {
        self.status
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
        let Some(compacted) = crate::compaction::compact(&self.messages) else {
            return Ok(self.context_stats());
        };
        let previous = std::mem::replace(&mut self.messages, compacted);
        if let Err(error) = self.checkpoint(false, self.messages.len()).await {
            self.messages = previous;
            return Err(error);
        }
        Ok(self.context_stats())
    }

    async fn auto_compact(&mut self, stable_len: &mut usize) -> Result<()> {
        let before = self.context_stats().approximate_tokens;
        if self
            .auto_compact_tokens
            .is_none_or(|threshold| before < threshold.get())
        {
            return Ok(());
        }
        let Some(compacted) = crate::compaction::compact(&self.messages) else {
            return Ok(());
        };
        let removed = self.messages.len() - compacted.len();
        // Never summarize the active turn: rollback and interrupted-session recovery
        // must retain an exact boundary between completed history and current work.
        if removed + 1 > *stable_len {
            return Ok(());
        }
        let previous = std::mem::replace(&mut self.messages, compacted);
        let after = self.context_stats().approximate_tokens;
        if after >= before {
            self.messages = previous;
            return Ok(());
        }
        let new_stable_len = *stable_len - removed;
        if let Err(error) = self.checkpoint(true, new_stable_len).await {
            self.messages = previous;
            return Err(error);
        }
        *stable_len = new_stable_len;
        self.output.emit(Event::Progress(format!(
            "Automatically compacted context: approximately {before} → {after} tokens"
        )));
        Ok(())
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
        self.status = SessionStatus::Ready;
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
            if let Some(journal) = store.load_journal(id.to_owned()).await? {
                saved.messages = journal.recover();
            } else {
                saved.messages.truncate(saved.stable_len);
            }
            saved.messages.push(Message::User { content: "[Harness notice: the previous run was interrupted. Completed results were retained. Tools with unknown execution may have changed local state; inspect before retrying.]".into() });
        }
        self.id = saved.id;
        self.messages = saved.messages;
        self.config.model = saved.selection.model;
        self.config.reasoning_effort = saved.selection.reasoning_effort;
        self.revision = saved.revision;
        self.status = if saved.interrupted {
            SessionStatus::Paused
        } else {
            saved.status
        };
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
                status: self.status,
                messages: self.messages.clone(),
                stable_len,
                revision: self.revision,
            })
            .await
            .context("Saving session checkpoint")?;
        Ok(())
    }

    /// Retain completed tool results on failure; never undo or replay local effects.
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
        let mut length = self.messages.len();
        self.status = SessionStatus::Running;
        let result = self.run_turn(prompt, &mut length, cancellation).await;
        if result.is_err() {
            self.output.emit(Event::AssistantAborted);
            crate::message::complete_pending_tools(&mut self.messages);
            if self
                .messages
                .iter()
                .skip(length)
                .any(|message| matches!(message, Message::Tool { .. }))
            {
                self.messages.push(Message::User { content: "[Harness notice: this turn stopped with an error. Completed tool results and local effects were retained. Inspect uncertain effects before continuing.]".into() });
            } else {
                self.messages.truncate(length);
            }
        }
        if matches!(result, Ok(SubmitOutcome::Cancelled)) {
            self.output.emit(Event::AssistantAborted);
            if self.messages.len() > length {
                self.messages.push(Message::User {
                    content: CANCELLED_NOTICE.into(),
                });
            }
        }
        self.status = match &result {
            Ok(SubmitOutcome::Completed) => SessionStatus::Ready,
            Ok(SubmitOutcome::Cancelled) => SessionStatus::Paused,
            Err(error) if error.downcast_ref::<TurnLimitReached>().is_some() => {
                SessionStatus::Paused
            }
            Err(_) => SessionStatus::Failed,
        };
        self.checkpoint(false, self.messages.len()).await?;
        if matches!(result, Ok(SubmitOutcome::Cancelled)) {
            self.output.emit(Event::Progress("Turn cancelled. Completed tool effects and results were retained; pending tools were skipped.".into()));
        }
        result
    }

    async fn run_turn(
        &mut self,
        prompt: String,
        stable_len: &mut usize,
        cancellation: &crate::cancellation::Cancellation,
    ) -> Result<SubmitOutcome> {
        if cancellation.is_cancelled() {
            return Ok(SubmitOutcome::Cancelled);
        }
        let prompt = self.skills.expand(prompt).await?;
        let prompt = self.config.lua.hook("before_prompt", prompt).await?;
        let prompt = if let Some(directory) = &self.instruction_directory {
            let instructions =
                crate::instructions::ProjectInstructions::load(directory.clone()).await?;
            self.output.emit(Event::Progress(format!(
                "Loaded {} project instruction files",
                instructions.paths.len()
            )));
            instructions.append_to(prompt)
        } else {
            prompt
        };
        if cancellation.is_cancelled() {
            return Ok(SubmitOutcome::Cancelled);
        }
        self.messages.push(Message::User { content: prompt });
        self.checkpoint(true, self.messages.len()).await?;
        let mut definitions = tools::definitions()
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        definitions.extend(self.config.lua.definitions.iter().cloned());
        for turn in 0..self.config.max_turns.get() {
            if cancellation.is_cancelled() {
                return Ok(SubmitOutcome::Cancelled);
            }
            self.auto_compact(stable_len).await?;
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
            let response = match self.request_with_retry(request, cancellation).await {
                Ok(response) => response,
                Err(_error) if cancellation.is_cancelled() => return Ok(SubmitOutcome::Cancelled),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to request {} model response", self.provider.name())
                    });
                }
            };
            let outcome = ResponseProcessor::new(response)
                .with_cancellation(cancellation.clone())
                .with_limits(self.config.limits)
                .with_runtime(self.output.clone(), Arc::clone(&self.config.lua))
                .with_journal(self.store.as_ref(), &self.id, self.revision)
                .process(&mut self.messages)
                .await?;
            if outcome == TurnOutcome::Finished {
                return Ok(SubmitOutcome::Completed);
            }
            if outcome == TurnOutcome::Cancelled {
                return Ok(SubmitOutcome::Cancelled);
            }
            self.checkpoint(true, self.messages.len()).await?;
            if cancellation.is_cancelled() {
                return Ok(SubmitOutcome::Cancelled);
            }
        }
        Err(TurnLimitReached(self.config.max_turns.get()).into())
    }

    async fn request_with_retry(
        &self,
        request: CompletionRequest<'_>,
        cancellation: &crate::cancellation::Cancellation,
    ) -> Result<crate::providers::Completion> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(anyhow::anyhow!("Request cancelled")),
                response = async {
                    if self.config.lua.has_after_response_hook {
                        self.provider.complete(request).await
                    } else {
                        self.provider.complete_stream(request, &self.output).await
                    }
                } => response,
            };
            match result {
                Ok(response) => return Ok(response),
                Err(error)
                    if attempt < MAX_PROVIDER_ATTEMPTS && retryable_provider_error(&error) =>
                {
                    let delay = std::time::Duration::from_millis(100 * (1_u64 << (attempt - 1)));
                    self.output.emit(Event::Progress(format!(
                        "Transient provider failure; retrying in {} ms ({}/{})...",
                        delay.as_millis(),
                        attempt,
                        MAX_PROVIDER_ATTEMPTS - 1
                    )));
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Err(anyhow::anyhow!("Request cancelled")),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
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

    struct RetryProvider {
        attempts: std::sync::Mutex<usize>,
    }
    impl Provider for RetryProvider {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn name(&self) -> &'static str {
            "Mock"
        }
        fn models(&self) -> ProviderFuture<'_, Vec<Model>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn complete<'a>(&'a self, _: CompletionRequest<'a>) -> ProviderFuture<'a, Completion> {
            Box::pin(async move {
                let mut attempts = self
                    .attempts
                    .lock()
                    .map_err(|_| anyhow::anyhow!("poisoned"))?;
                *attempts += 1;
                let attempt = *attempts;
                drop(attempts);
                if attempt < 3 {
                    bail!("HTTP 503 service unavailable")
                }
                Ok(serde_json::from_value(
                    serde_json::json!({"content":"Recovered"}),
                )?)
            })
        }
    }

    #[tokio::test]
    async fn retries_transient_provider_failures_and_emits_progress() -> Result<()> {
        let provider = Arc::new(RetryProvider {
            attempts: std::sync::Mutex::new(0),
        });
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut session = cancellation_session(provider.clone(), Output::channel(sender))?;
        session.submit("retry".into()).await?;
        assert_eq!(
            *provider
                .attempts
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?,
            3
        );
        let progress: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                Event::Progress(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            progress
                .iter()
                .filter(|text| text.contains("retrying"))
                .count(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn does_not_retry_permanent_provider_errors() -> Result<()> {
        let provider = Arc::new(MockProvider("mock"));
        let mut session = cancellation_session(provider, Output::default())?;
        assert!(session.submit("permanent".into()).await.is_err());
        Ok(())
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
    async fn turn_limit_retains_written_file_and_result_on_resume() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("changed");
        let provider = Arc::new(CancellationProvider {
            requests: std::sync::Mutex::default(),
            pending: false,
            first: serde_json::json!({"tool_calls":[{"id":"write", "type":"function", "function":{"name":"Write", "arguments":serde_json::json!({"file_path":path,"content":"kept"}).to_string()}}]}),
        });
        let store =
            SessionStore::open(directory.path().join("sessions.sqlite3"), directory.path())?;
        let mut session =
            cancellation_session(provider, Output::default())?.with_store(store.clone());
        session.config.max_turns = NonZeroUsize::MIN;
        assert!(session.submit("change file".into()).await.is_err());
        assert_eq!(session.status(), SessionStatus::Paused);
        assert_eq!(std::fs::read_to_string(&path)?, "kept");
        let id = session.id.clone();
        assert!(store.load_journal(id.clone()).await?.is_none());
        std::fs::remove_file(&path)?;
        session.resume(&id).await?;
        assert_eq!(session.status(), SessionStatus::Paused);
        assert!(session.messages.iter().any(|message| matches!(message, Message::Tool { tool_call_id, .. } if tool_call_id == "write")));
        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_journal_preserves_results_and_marks_uncertain_and_unstarted_calls()
    -> Result<()> {
        use crate::sessions::{JournalCall, ToolJournal};
        let directory = tempfile::tempdir()?;
        let store =
            SessionStore::open(directory.path().join("sessions.sqlite3"), directory.path())?;
        let mut session = cancellation_session(Arc::new(MockProvider("mock")), Output::default())?
            .with_store(store.clone());
        session.messages.push(Message::User {
            content: "task".into(),
        });
        session.checkpoint(true, 1).await?;
        let calls = ["done", "uncertain", "pending"].map(|id| serde_json::json!({"id":id,"type":"function","function":{"name":"Write","arguments":"{}"}}));
        let mut messages = session.messages.clone();
        messages.push(serde_json::from_value(
            serde_json::json!({"role":"assistant","tool_calls":calls}),
        )?);
        store
            .journal(
                session.id.clone(),
                session.revision,
                ToolJournal {
                    messages,
                    calls: vec![
                        JournalCall {
                            id: "done".into(),
                            started: true,
                            result: Some("written".into()),
                        },
                        JournalCall {
                            id: "uncertain".into(),
                            started: true,
                            result: None,
                        },
                        JournalCall {
                            id: "pending".into(),
                            started: false,
                            result: None,
                        },
                    ],
                },
            )
            .await?;
        let id = session.id.clone();
        assert!(session.resume(&id).await?);
        let history = serde_json::to_value(&session.messages)?;
        assert_eq!(
            history.pointer("/2/content"),
            Some(&serde_json::json!("written"))
        );
        let uncertain: serde_json::Value = serde_json::from_str(
            history
                .pointer("/3/content")
                .and_then(serde_json::Value::as_str)
                .context("uncertain result")?,
        )?;
        assert_eq!(
            uncertain.get("execution"),
            Some(&serde_json::json!("unknown"))
        );
        let pending: serde_json::Value = serde_json::from_str(
            history
                .pointer("/4/content")
                .and_then(serde_json::Value::as_str)
                .context("pending result")?,
        )?;
        assert_eq!(pending.get("executed"), Some(&serde_json::json!(false)));
        assert_eq!(session.status(), SessionStatus::Paused);
        Ok(())
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
    async fn auto_compaction_respects_threshold_and_preserves_recovery_boundary() -> Result<()> {
        for threshold in [None, NonZeroUsize::new(1), NonZeroUsize::new(1_000_000)] {
            let root = tempfile::tempdir()?;
            let store = SessionStore::open(root.path().join("sessions.sqlite3"), root.path())?;
            let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
            let mut session = cancellation_session(
                Arc::new(MockProvider("openrouter")),
                Output::channel(sender),
            )?
            .with_store(store.clone())
            .with_auto_compact_tokens(threshold);
            session.messages = vec![
                Message::User {
                    content: "old context ".repeat(1000)
                };
                10
            ];
            let mut stable = session.messages.len();
            session.messages.push(Message::User {
                content: "active task".into(),
            });
            session.auto_compact(&mut stable).await?;
            let enabled = threshold == NonZeroUsize::new(1);
            assert_eq!(session.messages.len(), if enabled { 7 } else { 11 });
            assert_eq!(stable, session.messages.len() - 1);
            assert!(
                matches!(session.messages.last(), Some(Message::User { content }) if content == "active task")
            );
            assert_eq!(events.try_recv().is_ok(), enabled);
            if enabled {
                let saved = store.load(session.id.clone()).await?;
                assert!(saved.interrupted);
                assert_eq!(saved.stable_len, stable);
                let mut restored =
                    cancellation_session(Arc::new(MockProvider("openrouter")), Output::default())?
                        .with_store(store.clone());
                assert!(restored.resume(&session.id).await?);
                assert_eq!(restored.messages.len(), stable + 1);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn auto_compaction_skips_size_increases_and_active_turn_boundaries() -> Result<()> {
        for (content, initial_stable) in [("x".into(), 8), ("large".repeat(1000), 0)] {
            let mut session =
                cancellation_session(Arc::new(MockProvider("openrouter")), Output::default())?
                    .with_auto_compact_tokens(NonZeroUsize::new(1));
            session.messages = vec![Message::User { content }; 8];
            let before = serde_json::to_value(&session.messages)?;
            let mut stable = initial_stable;
            session.auto_compact(&mut stable).await?;
            assert_eq!(serde_json::to_value(&session.messages)?, before);
            assert_eq!(stable, initial_stable);
        }
        Ok(())
    }

    #[tokio::test]
    async fn model_failure_after_auto_compaction_rolls_back_only_active_turn() -> Result<()> {
        let mut session =
            cancellation_session(Arc::new(MockProvider("openrouter")), Output::default())?
                .with_auto_compact_tokens(NonZeroUsize::new(1));
        session.messages = vec![
            Message::User {
                content: "old context ".repeat(1000)
            };
            10
        ];
        assert!(session.submit("active task".into()).await.is_err());
        assert_eq!(session.messages.len(), 6);
        assert!(!session.messages.iter().any(
            |message| matches!(message, Message::User { content } if content == "active task")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn auto_compaction_save_failure_preserves_history_and_boundary() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = SessionStore::open(root.path().join("sessions.sqlite3"), root.path())?;
        let mut session =
            cancellation_session(Arc::new(MockProvider("openrouter")), Output::default())?
                .with_store(store.clone())
                .with_auto_compact_tokens(NonZeroUsize::new(1));
        session.messages = vec![
            Message::User {
                content: "old context ".repeat(1000)
            };
            10
        ];
        let mut stable = session.messages.len();
        session.checkpoint(false, stable).await?;
        store.save(store.load(session.id.clone()).await?).await?;
        let original = serde_json::to_value(&session.messages)?;
        assert!(session.auto_compact(&mut stable).await.is_err());
        assert_eq!(stable, 10);
        assert_eq!(serde_json::to_value(&session.messages)?, original);
        Ok(())
    }

    #[tokio::test]
    async fn failed_compaction_save_restores_original_history() -> Result<()> {
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
        for index in 0..10 {
            session.messages.push(Message::User {
                content: format!("message {index}"),
            });
        }
        session.checkpoint(false, session.messages.len()).await?;
        let original = serde_json::to_value(&session.messages)?;
        let revision = session.revision;
        store.save(store.load(session.id.clone()).await?).await?;
        assert!(session.compact().await.is_err());
        assert_eq!(serde_json::to_value(&session.messages)?, original);
        assert_eq!(session.revision, revision);
        assert_eq!(
            serde_json::to_value(store.load(session.id.clone()).await?.messages)?,
            original
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
