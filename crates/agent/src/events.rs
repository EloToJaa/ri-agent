use std::{
    io::Write as _,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum Event {
    ModelRequest {
        usage: Option<crate::providers::TokenUsage>,
        finish_reason: Option<crate::providers::FinishReason>,
        elapsed_ms: u64,
    },
    Approval(ApprovalRequest),
    User(String),
    Progress(String),
    Assistant(String),
    /// Provisional text; Assistant carries the authoritative completed text.
    AssistantDelta(String),
    AssistantAborted,
    Tool(String),
    CommandOutput {
        command_id: String,
        stream: String,
        text: String,
    },
}

#[derive(Debug)]
pub struct ApprovalRequest {
    pub tool: String,
    pub id: String,
    pub arguments: String,
    pub preview: Option<String>,
    pub response: tokio::sync::oneshot::Sender<bool>,
}

#[derive(Clone, Default)]
pub struct Output {
    sender: Option<UnboundedSender<Event>>,
    streamed: Arc<Mutex<String>>,
    json: bool,
}

impl Output {
    pub async fn approve(&self, call: &crate::response::ToolCall, preview: Option<String>) -> bool {
        let (response, receiver) = tokio::sync::oneshot::channel();
        let request = ApprovalRequest {
            tool: call.function.name.clone(),
            id: call.id.clone(),
            arguments: call.function.arguments.clone().unwrap_or_default(),
            preview,
            response,
        };
        if let Some(sender) = &self.sender {
            if sender.send(Event::Approval(request)).is_err() {
                return false;
            }
            return receiver.await.unwrap_or(false);
        }
        tokio::task::spawn_blocking(move || {
            use std::io::IsTerminal as _;
            if !std::io::stdin().is_terminal() {
                eprintln!("Approval denied: stdin is not a terminal");
                return false;
            }
            eprintln!(
                "Approve {} ({})? Arguments: {}",
                request.tool.escape_debug(),
                request.id.escape_debug(),
                request.arguments.escape_debug()
            );
            if let Some(preview) = &request.preview {
                for line in preview.lines() {
                    eprintln!("{}", line.escape_debug());
                }
            }
            eprint!("Type yes to execute: ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).is_ok() && answer.trim() == "yes"
        })
        .await
        .unwrap_or(false)
    }
    pub fn json() -> Self {
        Self {
            json: true,
            ..Self::default()
        }
    }
    pub fn channel(sender: UnboundedSender<Event>) -> Self {
        Self {
            sender: Some(sender),
            ..Self::default()
        }
    }

    pub fn emit(&self, event: Event) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(event);
            return;
        }
        if self.json {
            let value = match event {
                Event::ModelRequest {
                    usage,
                    finish_reason,
                    elapsed_ms,
                } => {
                    serde_json::json!({"type":"model_request", "usage":usage, "finish_reason":finish_reason, "elapsed_ms":elapsed_ms})
                }
                Event::Approval(_) => return,
                Event::User(text) => serde_json::json!({"type":"user","text":text}),
                Event::Progress(text) => serde_json::json!({"type":"progress","text":text}),
                Event::Assistant(text) => serde_json::json!({"type":"assistant","text":text}),
                Event::AssistantDelta(text) => {
                    serde_json::json!({"type":"assistant_delta","text":text})
                }
                Event::AssistantAborted => serde_json::json!({"type":"assistant_aborted"}),
                Event::Tool(text) => serde_json::json!({"type":"tool","text":text}),
                Event::CommandOutput {
                    command_id,
                    stream,
                    text,
                } => {
                    serde_json::json!({"type":"command_output", "command_id":command_id, "stream":stream, "text":text})
                }
            };
            println!("{value}");
            return;
        }
        match event {
            Event::ModelRequest {
                usage,
                finish_reason,
                elapsed_ms,
            } => {
                if let Some(usage) = usage {
                    eprintln!(
                        "Model: {} input / {} output tokens, {elapsed_ms} ms, {finish_reason:?}",
                        usage.input_tokens, usage.output_tokens
                    );
                }
            }
            Event::CommandOutput { text, .. } => eprint!("{text}"),
            Event::Progress(text) => eprintln!("{text}"),
            Event::AssistantDelta(text) => {
                self.streamed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_str(&text);
                let mut stdout = std::io::stdout().lock();
                let _ = write!(stdout, "{text}");
                let _ = stdout.flush();
            }
            Event::Assistant(text) => {
                let mut streamed = self
                    .streamed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(rest) = text.strip_prefix(streamed.as_str()) {
                    println!("{rest}");
                } else {
                    println!("\n{text}");
                }
                streamed.clear();
            }
            Event::AssistantAborted => {
                let mut streamed = self
                    .streamed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !streamed.is_empty() {
                    println!();
                }
                streamed.clear();
            }
            Event::Approval(_) | Event::Tool(_) | Event::User(_) => {}
        }
    }
}
