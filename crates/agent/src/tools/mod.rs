mod bash;
pub mod commands;
mod discovery;
mod edit;
pub use discovery::{FileMatches, find_files};
mod read;
mod write;

use crate::{limits::Limits, response::ToolCall};
use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};
use std::{future::Future, pin::Pin};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Tool: Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a>;
}

// Register each tool here to enable both its API definition and execution.
static TOOLS: &[&dyn Tool] = &[
    &read::Read,
    &write::Write,
    &edit::Edit,
    &bash::Bash,
    &discovery::Search,
    &discovery::Find,
];

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    Function { function: FunctionDefinition },
}

#[derive(Serialize)]
pub struct FunctionDefinition {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

#[cfg(test)]
pub async fn execute_batch_cancellable(
    calls: &[ToolCall],
    limits: Limits,
    output: &crate::events::Output,
    lua: Option<&std::sync::Arc<crate::config::LuaConfig>>,
    cancellation: &crate::cancellation::Cancellation,
) -> Vec<String> {
    execute_batch_managed(
        calls,
        limits,
        output,
        lua,
        cancellation,
        &commands::Commands::default(),
    )
    .await
}

pub async fn execute_batch_managed(
    calls: &[ToolCall],
    limits: Limits,
    output: &crate::events::Output,
    lua: Option<&std::sync::Arc<crate::config::LuaConfig>>,
    cancellation: &crate::cancellation::Cancellation,
    commands: &commands::Commands,
) -> Vec<String> {
    execute_batch_with(calls, |call| async move {
        // Let the frontend process pending input at each tool boundary.
        tokio::task::yield_now().await;
        if cancellation.is_cancelled() {
            return Ok(json!({"cancelled":true,"executed":false,"error":"Tool skipped because the user cancelled the turn"}).to_string());
        }
        if limits.ask && !limits.read_only && !matches!(call.function.name.as_str(), "Read" | "Search" | "Find") {
            let approved = tokio::select! {
                biased;
                () = cancellation.cancelled() => false,
                approved = output.approve(call) => approved,
            };
            if !approved || cancellation.is_cancelled() {
                return Ok(json!({"executed":false,"cancelled":cancellation.is_cancelled(),"error":"Tool approval denied"}).to_string());
            }
        }
        // Do not drop an in-flight filesystem operation or trusted Lua callback.
        if call.function.name == "Bash" && call.r#type == "function" && !limits.read_only && !lua.is_some_and(|lua| lua.has_tool("Bash")) {
            output.emit(crate::events::Event::Progress(format!("Running Bash ({})...", call.id)));
            return bash::execute_managed(call.function.arguments.as_deref().context("Missing Bash arguments")?, limits, commands, output, cancellation).await;
        }
        execute_with(call, limits, output, lua).await
    })
        .await
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|error| json!({"error": format!("{error:#}")}).to_string())
        })
        .collect()
}

async fn execute_batch_with<'a, F, Fut>(
    mut calls: &'a [ToolCall],
    mut execute: F,
) -> Vec<Result<String>>
where
    F: FnMut(&'a ToolCall) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let mut results = Vec::with_capacity(calls.len());
    while let Some((first, rest)) = calls.split_first() {
        let reads = calls
            .iter()
            .take_while(|call| call.r#type == "function" && call.function.name == "Read")
            .count();
        if reads == 0 {
            results.push(execute(first).await);
            calls = rest;
            continue;
        }
        let (batch, rest) = calls.split_at(reads);
        // Buffered preserves request order while polling up to four reads together.
        results.extend(
            stream::iter(batch.iter().map(&mut execute))
                .buffered(4)
                .collect::<Vec<_>>()
                .await,
        );
        calls = rest;
    }
    results
}

pub fn definitions() -> Vec<ToolDefinition> {
    TOOLS
        .iter()
        .map(|tool| ToolDefinition::Function {
            function: FunctionDefinition {
                name: tool.name(),
                description: tool.description(),
                parameters: tool.parameters(),
            },
        })
        .collect()
}

#[cfg(test)]
pub async fn execute(call: &ToolCall, limits: Limits) -> Result<String> {
    execute_with(call, limits, &crate::events::Output::default(), None).await
}

async fn execute_with(
    call: &ToolCall,
    limits: Limits,
    output: &crate::events::Output,
    lua: Option<&std::sync::Arc<crate::config::LuaConfig>>,
) -> Result<String> {
    if limits.read_only && !matches!(call.function.name.as_str(), "Read" | "Search" | "Find") {
        let error = format!("Tool {} blocked by read-only policy", call.function.name);
        output.emit(crate::events::Event::Progress(error.clone()));
        return Err(anyhow::anyhow!(error));
    }
    output.emit(crate::events::Event::Progress(format!(
        "Running {} ({})...",
        call.function.name, call.id
    )));
    let result = async {
        if call.r#type == "function"
            && let Some(lua) = lua.filter(|lua| lua.has_tool(&call.function.name))
        {
            let arguments = call
                .function
                .arguments
                .clone()
                .context("Missing Lua tool arguments")?;
            let result = lua.execute(call.function.name.clone(), arguments).await?;
            return crate::limits::read_output(result.as_bytes(), limits.max_output_bytes).await;
        }
        execute_inner(call, limits).await
    }
    .await;
    let status = match &result {
        Ok(_) => "completed",
        Err(_) => "failed",
    };
    output.emit(crate::events::Event::Progress(format!(
        "{} ({}) {status}",
        call.function.name, call.id
    )));
    result
}

async fn execute_inner(call: &ToolCall, limits: Limits) -> Result<String> {
    if call.r#type != "function" {
        bail!("Unsupported tool call type: {}", call.r#type);
    }

    let tool = TOOLS
        .iter()
        .find(|tool| tool.name() == call.function.name)
        .with_context(|| format!("Unknown tool: {}", call.function.name))?;
    let arguments = call
        .function
        .arguments
        .as_deref()
        .with_context(|| format!("Tool call {} is missing arguments", call.id))?;

    tool.execute(arguments, limits)
        .await
        .with_context(|| format!("Tool call {} ({}) failed", call.id, tool.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approvals_control_writes_and_fail_closed() -> Result<()> {
        use crate::{
            cancellation::Cancellation,
            events::{Event, Output},
        };
        for decision in ["approve", "deny", "drop", "cancel", "read_only"] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("result");
            let arguments = json!({"file_path": path, "content": "approved content"}).to_string();
            let calls = [serde_json::from_value(
                json!({"id":"write-1", "type":"function", "function":{"name":"Write", "arguments":arguments}}),
            )?];
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let output = Output::channel(sender);
            let cancellation = Cancellation::default();
            let limits = Limits {
                ask: true,
                read_only: decision == "read_only",
                ..Limits::default()
            };
            let work = execute_batch_cancellable(&calls, limits, &output, None, &cancellation);
            let respond = async {
                if decision == "read_only" {
                    return Ok::<_, anyhow::Error>(());
                }
                let Some(Event::Approval(request)) = receiver.recv().await else {
                    bail!("missing approval");
                };
                assert_eq!(request.tool, "Write");
                assert_eq!(request.id, "write-1");
                assert_eq!(request.arguments, arguments);
                match decision {
                    "approve" => {
                        assert!(request.response.send(true).is_ok());
                    }
                    "deny" => {
                        assert!(request.response.send(false).is_ok());
                    }
                    "cancel" => {
                        cancellation.cancel();
                        assert!(request.response.send(true).is_ok());
                    }
                    _ => drop(request),
                }
                Ok(())
            };
            let (results, response) = Box::pin(tokio::time::timeout(
                std::time::Duration::from_secs(2),
                async { tokio::join!(work, respond) },
            ))
            .await?;
            response?;
            assert_eq!(
                path.exists(),
                decision == "approve",
                "{decision}: {results:?}"
            );
            if decision == "approve" {
                assert_eq!(std::fs::read_to_string(path)?, "approved content");
            }
            if decision == "read_only" {
                while let Ok(event) = receiver.try_recv() {
                    assert!(!matches!(event, Event::Approval(_)));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn overlaps_reads_with_a_bound_and_serializes_mutations() -> Result<()> {
        use std::cell::{Cell, RefCell};
        let names = [
            "Read", "Read", "Read", "Read", "Read", "Write", "Read", "Bash", "Read",
        ];
        let calls: Vec<ToolCall> = names
            .iter()
            .enumerate()
            .map(|(id, name)| {
                serde_json::from_value(json!({
            "id": id.to_string(), "type": "function", "function": {"name": name, "arguments": "{}"}
        }))
            })
            .collect::<Result<_, _>>()?;
        let active = Cell::new(0);
        let peak = Cell::new(0);
        let completed = RefCell::new(Vec::new());
        let results = execute_batch_with(&calls, |call| {
            let active = &active;
            let peak = &peak;
            let completed = &completed;
            async move {
                let id: usize = call.id.parse()?;
                if call.function.name != "Read" {
                    assert_eq!(active.get(), 0);
                    assert_eq!(completed.borrow().len(), id);
                }
                if id > 5 {
                    assert!(completed.borrow().contains(&5));
                }
                if id > 7 {
                    assert!(completed.borrow().contains(&7));
                }
                active.set(active.get() + 1);
                peak.set(peak.get().max(active.get()));
                tokio::time::sleep(std::time::Duration::from_millis(10 - id as u64)).await;
                active.set(active.get() - 1);
                completed.borrow_mut().push(id);
                Ok(call.id.clone())
            }
        })
        .await;
        assert_eq!(peak.get(), 4);
        assert_eq!(
            results.into_iter().collect::<Result<Vec<_>>>()?,
            calls.iter().map(|call| call.id.clone()).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn advertised_read_tool_can_be_executed() -> Result<()> {
        let definition = definitions()
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|definition| {
                definition.pointer("/function/name").and_then(Value::as_str) == Some("Read")
            })
            .context("Missing Read definition")?;
        assert_eq!(
            definition.pointer("/function/parameters/required"),
            Some(&json!(["file_path"]))
        );
        let call: ToolCall = serde_json::from_value(json!({
            "id": "read_1",
            "type": definition.get("type"),
            "function": {
                "name": definition.pointer("/function/name"),
                "arguments": json!({
                    "file_path": concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/read.rs")
                }).to_string()
            }
        }))?;
        assert_eq!(
            execute(&call, Limits::default()).await?,
            include_str!("read.rs")
        );
        Ok(())
    }

    #[tokio::test]
    async fn read_only_policy_blocks_mutations_before_execution() -> Result<()> {
        let limits = Limits {
            read_only: true,
            ..Limits::default()
        };
        let call: ToolCall = serde_json::from_value(json!({
            "id":"edit_1","type":"function","function":{"name":"Edit","arguments":"{}"}
        }))?;
        let error = execute(&call, limits)
            .await
            .err()
            .context("Edit must be blocked")?;
        assert!(error.to_string().contains("read-only policy"));
        let read: ToolCall = serde_json::from_value(json!({
            "id":"read_1","type":"function","function":{"name":"Read","arguments":"{}"}
        }))?;
        assert!(execute(&read, limits).await.is_err());
        Ok(())
    }
}
