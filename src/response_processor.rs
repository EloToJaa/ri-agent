use crate::{message::Message, response::Response, tools};
use anyhow::{Result, bail};
use tokio::io::{self, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnOutcome {
    Continue,
    Finished,
}

pub(crate) struct ResponseProcessor {
    response: Response,
    limits: crate::limits::Limits,
}

impl ResponseProcessor {
    pub(crate) fn with_limits(mut self, limits: crate::limits::Limits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn new(response: Response) -> Self {
        Self {
            response,
            limits: crate::limits::Limits::default(),
        }
    }

    pub(crate) async fn process(&self, messages: &mut Vec<Message>) -> Result<TurnOutcome> {
        let mut output = io::stdout();
        let outcome = self.process_to(messages, &mut output).await?;
        output.flush().await?;
        Ok(outcome)
    }

    async fn process_to(
        &self,
        messages: &mut Vec<Message>,
        output: &mut (impl AsyncWrite + Unpin),
    ) -> Result<TurnOutcome> {
        let Some(choice) = self.response.choices.first() else {
            bail!("Response contains no choices");
        };

        messages.push(Message::Assistant {
            content: choice.message.content.clone(),
            tool_calls: choice.message.tool_calls.clone(),
        });

        if choice.message.tool_calls.is_empty() {
            if let Some(content) = &choice.message.content {
                output.write_all(format!("{content}\n").as_bytes()).await?;
            }
            return Ok(TurnOutcome::Finished);
        }

        let results = tools::execute_batch(&choice.message.tool_calls, self.limits).await;
        for (call, contents) in choice.message.tool_calls.iter().zip(results) {
            messages.push(Message::Tool {
                tool_call_id: call.id.clone(),
                content: contents,
            });
        }

        Ok(TurnOutcome::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn processor(message: serde_json::Value) -> ResponseProcessor {
        ResponseProcessor::new(
            serde_json::from_value(json!({"choices": [{"message": message}]})).unwrap(),
        )
    }

    #[tokio::test]
    async fn prints_normal_message_without_tool_calls() {
        let mut output = Vec::new();
        assert_eq!(
            processor(json!({"content": "Hello"}))
                .process_to(&mut Vec::new(), &mut output)
                .await
                .unwrap(),
            TurnOutcome::Finished
        );
        assert_eq!(output, b"Hello\n");
    }

    #[tokio::test]
    async fn reads_multiple_files_with_or_without_text_content() {
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
            second_call["id"] = json!("read_2");
            assert_eq!(
                processor(
                    json!({"content": content, "tool_calls": [call.clone(), second_call.clone()]})
                )
                .process_to(&mut messages, &mut output)
                .await
                .unwrap(),
                TurnOutcome::Continue
            );
            assert!(output.is_empty());
            let history = serde_json::to_value(&messages).unwrap();
            assert_eq!(
                history[0],
                json!({"role": "user", "content": "Read the file"})
            );
            assert_eq!(
                history[1],
                json!({"role": "assistant", "content": content, "tool_calls": [call, second_call]})
            );
            for (index, id) in [(2, "read_1"), (3, "read_2")] {
                assert_eq!(
                    history[index],
                    json!({"role": "tool", "tool_call_id": id, "content": include_str!("response.rs")})
                );
            }
            assert_eq!(
                processor(json!({"content": "Done"}))
                    .process_to(&mut messages, &mut output)
                    .await
                    .unwrap(),
                TurnOutcome::Finished
            );
            assert_eq!(output, b"Done\n");
            assert_eq!(
                serde_json::to_value(&messages).unwrap()[4],
                json!({"role": "assistant", "content": "Done"})
            );
        }
    }

    #[tokio::test]
    async fn returns_tool_errors_to_the_model() {
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
            let response = processor(json!({"tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}));
            let mut messages = Vec::new();
            assert_eq!(
                response
                    .process_to(&mut messages, &mut Vec::new())
                    .await
                    .unwrap(),
                TurnOutcome::Continue
            );
            let Message::Tool {
                content,
                tool_call_id,
            } = &messages[1]
            else {
                panic!("Expected a tool result");
            };
            assert_eq!(tool_call_id, "call_1");
            let error: serde_json::Value = serde_json::from_str(content).unwrap();
            assert!(!error["error"].as_str().unwrap().is_empty());
        }
    }
}
