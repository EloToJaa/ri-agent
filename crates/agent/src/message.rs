use crate::response::ToolCall;
use serde::{Deserialize, Serialize};

pub fn complete_pending_tools(messages: &mut Vec<Message>) {
    let Some(index) = messages
        .iter()
        .rposition(|message| matches!(message, Message::Assistant { .. }))
    else {
        return;
    };
    let Some(Message::Assistant { tool_calls, .. }) = messages.get(index) else {
        return;
    };
    let missing: Vec<_> = tool_calls.iter().filter(|call| !messages.iter().skip(index + 1).any(|message| matches!(message, Message::Tool { tool_call_id, .. } if tool_call_id == &call.id))).map(|call| call.id.clone()).collect();
    for id in missing {
        messages.push(Message::Tool { tool_call_id: id, content: serde_json::json!({"execution":"unknown", "error":"No result was recorded. Inspect local state before retrying."}).to_string() });
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_details: Option<Vec<serde_json::Value>>,
    },
    #[serde(rename = "tool")]
    Tool {
        content: String,
        tool_call_id: String,
    },
}
