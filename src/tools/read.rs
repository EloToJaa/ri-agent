use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncReadExt;

pub(super) struct Read;

#[derive(Deserialize)]
struct Arguments {
    file_path: PathBuf,
}

impl Tool for Read {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn description(&self) -> &'static str {
        "Read and return the contents of a file"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to read"
                }
            },
            "required": ["file_path"]
        })
    }

    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments: Arguments =
                serde_json::from_str(arguments).context("Invalid Read arguments")?;
            let file = fs::File::open(&arguments.file_path)
                .await
                .with_context(|| format!("Failed to read {}", arguments.file_path.display()))?;
            crate::limits::read_output(
                file.take((limits.max_output_bytes.get() as u64).saturating_add(1)),
                limits.max_output_bytes,
            )
            .await
        })
    }
}
