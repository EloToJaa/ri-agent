use std::{
    io::Write as _,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum Event {
    User(String),
    Progress(String),
    Assistant(String),
    /// Provisional text; Assistant carries the authoritative completed text.
    AssistantDelta(String),
    AssistantAborted,
    Tool(String),
}

#[derive(Clone, Default)]
pub struct Output {
    sender: Option<UnboundedSender<Event>>,
    streamed: Arc<Mutex<String>>,
    json: bool,
}

impl Output {
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
                Event::User(text) => serde_json::json!({"type":"user","text":text}),
                Event::Progress(text) => serde_json::json!({"type":"progress","text":text}),
                Event::Assistant(text) => serde_json::json!({"type":"assistant","text":text}),
                Event::AssistantDelta(text) => {
                    serde_json::json!({"type":"assistant_delta","text":text})
                }
                Event::AssistantAborted => serde_json::json!({"type":"assistant_aborted"}),
                Event::Tool(text) => serde_json::json!({"type":"tool","text":text}),
            };
            println!("{value}");
            return;
        }
        match event {
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
            Event::Tool(_) | Event::User(_) => {}
        }
    }
}
