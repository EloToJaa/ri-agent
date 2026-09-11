use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct Response {
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<crate::providers::TokenUsage>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: Message,
    #[serde(default)]
    pub finish_reason: Option<crate::providers::FinishReason>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub usage: Option<crate::providers::TokenUsage>,
    #[serde(default)]
    pub finish_reason: crate::providers::FinishReason,
    pub content: Option<String>,
    #[serde(default, alias = "reasoning_content")]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_details: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: Option<String>,
}
