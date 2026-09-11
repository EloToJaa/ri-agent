use crate::message::Message;
use std::fmt::Write as _;

const KEEP: usize = 6;
const SUMMARY_CHARS: usize = 8192;
const HEADER: &str = "[Conversation summary; older messages compacted]\n";

/// Preserve both the beginning and conclusion, including errors at the end of logs.
pub fn excerpt(content: &str, limit: usize) -> String {
    if content.chars().count() <= limit {
        return content.to_owned();
    }
    let head: String = content.chars().take(limit / 2).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(limit / 2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n[... excerpt; full content in archive ...]\n{tail}")
}

/// Retain at least six messages, moving the boundary back to a user turn so
/// assistant tool calls and their results always remain together.
pub fn compact(messages: &[Message]) -> Option<Vec<Message>> {
    let latest_split = messages.len().checked_sub(KEEP)?;
    let split = messages
        .get(..=latest_split)?
        .iter()
        .rposition(|message| matches!(message, Message::User { .. }))?;
    if split <= 1 {
        return None;
    }
    let mut remaining = SUMMARY_CHARS - HEADER.chars().count();
    let mut excerpts = Vec::new();
    for message in messages.get(..split)?.iter().rev() {
        let (label, content) = match message {
            Message::User { content } => ("Request / constraints", content.clone()),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => (
                "Decisions / progress / remaining work (assistant)",
                format!(
                    "{}{}",
                    content.as_deref().unwrap_or_default(),
                    tool_calls.iter().fold(String::new(), |mut text, call| {
                        let _ = write!(
                            text,
                            "\nTool {} ({}): {}",
                            call.function.name,
                            call.id,
                            excerpt(call.function.arguments.as_deref().unwrap_or_default(), 200)
                        );
                        text
                    })
                ),
            ),
            Message::Tool {
                content,
                tool_call_id,
            } => ("Observed tool result", format!("{tool_call_id}: {content}")),
        };
        let excerpt = format!("{label}: {}\n", excerpt(&content, 800));
        let length = excerpt.chars().count();
        if length > remaining {
            break;
        }
        remaining -= length;
        excerpts.push(excerpt);
    }
    excerpts.reverse();
    let mut compacted = Vec::with_capacity(messages.len() - split + 1);
    compacted.push(Message::User {
        content: format!("{HEADER}{}", excerpts.concat()),
    });
    compacted.extend_from_slice(messages.get(split..)?);
    Some(compacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_a_whole_tool_turn_and_bounds_unicode_summary() -> anyhow::Result<()> {
        let mut messages = vec![
            Message::User {
                content: "世界".repeat(500)
            };
            1000
        ];
        let turn_start = messages.len();
        messages.push(Message::User {
            content: "latest task".into(),
        });
        let calls: Vec<_> = (0..6).map(|index| serde_json::json!({"id":index.to_string(),"type":"function","function":{"name":"Read","arguments":"{}"}})).collect();
        messages.push(serde_json::from_value(
            serde_json::json!({"role":"assistant", "content":null, "tool_calls":calls}),
        )?);
        for index in 0..6 {
            messages.push(Message::Tool {
                content: "result".into(),
                tool_call_id: index.to_string(),
            });
        }
        let compacted = compact(&messages).ok_or_else(|| anyhow::anyhow!("expected compaction"))?;
        assert_eq!(
            serde_json::to_value(compacted.get(1..))?,
            serde_json::to_value(messages.get(turn_start..))?
        );
        let Some(Message::User { content }) = compacted.first() else {
            anyhow::bail!("missing summary")
        };
        assert!(content.chars().count() <= SUMMARY_CHARS);
        assert!(compact(&compacted).is_none());
        Ok(())
    }

    #[test]
    fn leaves_short_histories_and_single_long_turns_unchanged() {
        assert!(compact(&[]).is_none());
        let mut messages = vec![Message::User {
            content: "task".into(),
        }];
        messages.extend((0..12).map(|_| Message::Assistant {
            content: Some("reply".into()),
            tool_calls: vec![],
            reasoning: None,
            reasoning_details: None,
        }));
        assert!(compact(&messages).is_none());
    }
}
