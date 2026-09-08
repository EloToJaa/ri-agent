use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum Event {
    Progress(String),
    Assistant(String),
    Tool(String),
}

#[derive(Clone, Default)]
pub struct Output(Option<UnboundedSender<Event>>);

impl Output {
    pub fn channel(sender: UnboundedSender<Event>) -> Self {
        Self(Some(sender))
    }

    pub fn emit(&self, event: Event) {
        if let Some(sender) = &self.0 {
            let _ = sender.send(event);
            return;
        }
        match event {
            Event::Progress(text) => eprintln!("{text}"),
            Event::Assistant(text) => println!("{text}"),
            Event::Tool(_) => {}
        }
    }
}
