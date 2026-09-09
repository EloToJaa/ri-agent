mod bash;
mod discovery;
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

pub async fn execute_batch(
    calls: &[ToolCall],
    limits: Limits,
    output: &crate::events::Output,
    lua: Option<&std::sync::Arc<crate::config::LuaConfig>>,
) -> Vec<String> {
    execute_batch_with(calls, |call| execute_with(call, limits, output, lua))
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
}
