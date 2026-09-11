use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Incomplete,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TokenUsage {
    #[serde(default, alias = "prompt_tokens")]
    pub input_tokens: u64,
    #[serde(default, alias = "completion_tokens")]
    pub output_tokens: u64,
    #[serde(default, alias = "cost")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Metrics {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reported_cost_usd: f64,
    pub requests_without_usage: u64,
    pub elapsed_ms: u64,
}

impl Metrics {
    pub fn record(&mut self, usage: Option<&TokenUsage>, elapsed_ms: u64) {
        self.requests = self.requests.saturating_add(1);
        self.elapsed_ms = self.elapsed_ms.saturating_add(elapsed_ms);
        if let Some(usage) = usage {
            self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
            self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
            self.reported_cost_usd += usage
                .cost_usd
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .unwrap_or_default();
        } else {
            self.requests_without_usage = self.requests_without_usage.saturating_add(1);
        }
    }
}
