use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(crate) struct Response {
    pub(crate) choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    pub(crate) message: Message,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Message {
    pub(crate) content: Option<String>,
    #[serde(default, alias = "reasoning_content")]
    pub(crate) reasoning: Option<String>,
    #[serde(default)]
    pub(crate) reasoning_details: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) r#type: String,
    pub(crate) function: ToolCallFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ToolCallFunction {
    pub(crate) name: String,
    pub(crate) arguments: Option<String>,
}
