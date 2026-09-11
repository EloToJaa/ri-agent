use crate::{message::Message, providers::Completion, tools};
use anyhow::{Context, Result};
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Continue,
    Finished,
    Cancelled,
}

pub struct ResponseProcessor {
    response: Completion,
    limits: crate::limits::Limits,
    output: crate::events::Output,
    lua: Option<std::sync::Arc<crate::config::LuaConfig>>,
    cancellation: crate::cancellation::Cancellation,
    journal: Option<(crate::sessions::SessionStore, String, i64)>,
    commands: std::sync::Arc<tools::commands::Commands>,
}

impl ResponseProcessor {
    pub(crate) fn with_commands(
        mut self,
        commands: std::sync::Arc<tools::commands::Commands>,
    ) -> Self {
        self.commands = commands;
        self
    }
    pub(crate) fn with_journal(
        mut self,
        store: Option<&crate::sessions::SessionStore>,
        id: &str,
        revision: i64,
    ) -> Self {
        self.journal = store.map(|store| (store.clone(), id.to_owned(), revision));
        self
    }
    pub(crate) fn with_cancellation(
        mut self,
        cancellation: crate::cancellation::Cancellation,
    ) -> Self {
        self.cancellation = cancellation;
        self
    }
    pub(crate) const fn with_limits(mut self, limits: crate::limits::Limits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn with_runtime(
        mut self,
        output: crate::events::Output,
        lua: std::sync::Arc<crate::config::LuaConfig>,
    ) -> Self {
        self.output = output;
        self.lua = Some(lua);
        self
    }

    pub(crate) fn new(response: Completion) -> Self {
        Self {
            response,
            limits: crate::limits::Limits::default(),
            output: crate::events::Output::default(),
            lua: None,
            cancellation: crate::cancellation::Cancellation::default(),
            journal: None,
            commands: std::sync::Arc::default(),
        }
    }

    pub(crate) async fn process(&self, messages: &mut Vec<Message>) -> Result<TurnOutcome> {
        self.process_to(messages, &mut tokio::io::sink()).await
    }

    async fn process_to(
        &self,
        messages: &mut Vec<Message>,
        output: &mut (impl AsyncWrite + Unpin),
    ) -> Result<TurnOutcome> {
        let content = match (&self.lua, &self.response.content) {
            (Some(lua), Some(content)) => Some(lua.hook("after_response", content.clone()).await?),
            _ => self.response.content.clone(),
        };
        if self.cancellation.is_cancelled() {
            return Ok(TurnOutcome::Cancelled);
        }
        if let Some(text) = &content {
            self.output
                .emit(crate::events::Event::Assistant(text.clone()));
        }
        messages.push(Message::Assistant {
            content: content.clone(),
            tool_calls: self.response.tool_calls.clone(),
            reasoning: self.response.reasoning.clone(),
            reasoning_details: self.response.reasoning_details.clone(),
        });

        if self.response.tool_calls.is_empty() {
            if let Some(content) = &content {
                output.write_all(format!("{content}\n").as_bytes()).await?;
            }
            return Ok(TurnOutcome::Finished);
        }

        let results = self.execute_tools(messages).await?;
        for (call, contents) in self.response.tool_calls.iter().zip(results) {
            self.output.emit(crate::events::Event::Tool(format!(
                "{} ({}):\n{}",
                call.function.name, call.id, contents
            )));
            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: contents,
            });
        }
        Ok(if self.cancellation.is_cancelled() {
            TurnOutcome::Cancelled
        } else {
            TurnOutcome::Continue
        })
    }

    async fn execute_tools(&self, messages: &mut Vec<Message>) -> Result<Vec<String>> {
        Ok(if let Some((store, id, revision)) = &self.journal {
            let mut journal = crate::sessions::ToolJournal {
                messages: messages.clone(),
                calls: self
                    .response
                    .tool_calls
                    .iter()
                    .map(|call| crate::sessions::JournalCall {
                        id: call.id.clone(),
                        started: false,
                        result: None,
                    })
                    .collect(),
            };
            store
                .journal(id.clone(), *revision, journal.clone())
                .await?;
            let mut results = Vec::new();
            while results.len() < self.response.tool_calls.len() {
                let start = results.len();
                let remaining = self
                    .response
                    .tool_calls
                    .get(start..)
                    .context("Invalid tool batch")?;
                let reads = remaining
                    .iter()
                    .take_while(|call| call.function.name == "Read")
                    .count();
                let end = start + reads.max(1);
                for entry in journal.calls.iter_mut().take(end).skip(start) {
                    entry.started = true;
                }
                if let Err(error) = store.journal(id.clone(), *revision, journal.clone()).await {
                    *messages = journal.recover();
                    return Err(error);
                }
                let batch = tools::execute_batch_managed(
                    self.response
                        .tool_calls
                        .get(start..end)
                        .context("Invalid tool batch")?,
                    self.limits,
                    &self.output,
                    self.lua.as_ref(),
                    &self.cancellation,
                    &self.commands,
                )
                .await;
                for (entry, result) in journal.calls.iter_mut().take(end).skip(start).zip(&batch) {
                    entry.result = Some(result.clone());
                }
                results.extend(batch);
                if let Err(error) = store.journal(id.clone(), *revision, journal.clone()).await {
                    *messages = journal.recover();
                    return Err(error);
                }
            }
            results
        } else {
            tools::execute_batch_managed(
                &self.response.tool_calls,
                self.limits,
                &self.output,
                self.lua.as_ref(),
                &self.cancellation,
                &self.commands,
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, bail};
    use serde_json::json;

    fn processor(message: &serde_json::Value) -> Result<ResponseProcessor> {
        Ok(ResponseProcessor::new(serde_json::from_value(
            message.clone(),
        )?))
    }

    #[tokio::test]
    async fn prints_normal_message_without_tool_calls() -> Result<()> {
        let mut output = Vec::new();
        assert_eq!(
            processor(&json!({"content": "Hello"}))?
                .process_to(&mut Vec::new(), &mut output)
                .await?,
            TurnOutcome::Finished
        );
        assert_eq!(output, b"Hello\n");
        Ok(())
    }

    #[tokio::test]
    async fn reads_multiple_files_with_or_without_text_content() -> Result<()> {
        let file_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/response.rs");
        let call = json!({
            "id": "read_1",
            "type": "function",
            "function": {
                "name": "Read",
                "arguments": json!({"file_path": file_path}).to_string()
            }
        });

        for content in [None, Some("Reading files")] {
            let mut output = Vec::new();
            let mut messages = vec![Message::User {
                content: "Read the file".into(),
            }];
            let mut second_call = call.clone();
            *second_call.get_mut("id").context("Missing call ID")? = json!("read_2");
            assert_eq!(
                processor(
                    &json!({"content": content, "tool_calls": [call.clone(), second_call.clone()]})
                )?
                .process_to(&mut messages, &mut output)
                .await?,
                TurnOutcome::Continue
            );
            assert!(output.is_empty());
            let history = serde_json::to_value(&messages)?;
            assert_eq!(
                history.get(0),
                Some(&json!({"role": "user", "content": "Read the file"}))
            );
            assert_eq!(
                history.get(1),
                Some(
                    &json!({"role": "assistant", "content": content, "tool_calls": [call, second_call]})
                )
            );
            for (index, id) in [(2, "read_1"), (3, "read_2")] {
                assert_eq!(
                    history.get(index),
                    Some(
                        &json!({"role": "tool", "tool_call_id": id, "content": include_str!("response.rs")})
                    )
                );
            }
            assert_eq!(
                processor(&json!({"content": "Done"}))?
                    .process_to(&mut messages, &mut output)
                    .await?,
                TurnOutcome::Finished
            );
            assert_eq!(output, b"Done\n");
            assert_eq!(
                serde_json::to_value(&messages)?.get(4),
                Some(&json!({"role": "assistant", "content": "Done"}))
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn returns_tool_errors_to_the_model() -> Result<()> {
        for (name, arguments) in [
            ("Read", None),
            ("Read", Some("{}")),
            ("Read", Some("invalid")),
            ("Write", Some("{}")),
            ("Unknown", Some("{}")),
            (
                "Read",
                Some(r#"{"file_path":"/nonexistent-agent-test-file"}"#),
            ),
        ] {
            let response = processor(&json!({"tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}))?;
            let mut messages = Vec::new();
            assert_eq!(
                response.process_to(&mut messages, &mut Vec::new()).await?,
                TurnOutcome::Continue
            );
            let Message::Tool {
                content,
                tool_call_id,
            } = messages.get(1).context("Missing tool result")?
            else {
                bail!("Expected a tool result");
            };
            assert_eq!(tool_call_id, "call_1");
            let error: serde_json::Value = serde_json::from_str(content)?;
            assert!(
                !error
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .context("Missing error text")?
                    .is_empty()
            );
        }
        Ok(())
    }
}
