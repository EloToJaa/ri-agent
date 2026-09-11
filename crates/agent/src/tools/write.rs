use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
#[cfg(test)]
use tokio::fs;

pub(super) struct Write;

#[derive(Deserialize)]
struct Arguments {
    file_path: PathBuf,
    content: String,
    expected_sha256: Option<String>,
}

pub(super) async fn prepare(
    arguments: &str,
    limits: Limits,
) -> anyhow::Result<super::files::Mutation> {
    let args: Arguments = serde_json::from_str(arguments).context("Invalid Write arguments")?;
    let snapshot = super::files::Snapshot::read(
        args.file_path.clone(),
        true,
        args.expected_sha256.as_deref(),
    )
    .await?;
    let patch = super::edit::diff(
        &args.file_path.to_string_lossy(),
        snapshot.content.as_deref().unwrap_or_default(),
        &args.content,
    );
    let patch = crate::limits::read_output(patch.as_bytes(), limits.max_output_bytes).await?;
    snapshot.prepare(args.content, format!("File written successfully\n{patch}"))
}

impl Tool for Write {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, creating it or overwriting its existing contents"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expected_sha256": {"type":"string", "description":"Read's sha256, or missing to require a new file"},
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move { prepare(arguments, limits).await?.apply().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn creates_and_overwrites_a_file() -> anyhow::Result<()> {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory =
            std::env::temp_dir().join(format!("write-tool-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).await?;
        let path = directory.join("output.txt");

        for content in ["Hello, world!", "Short", ""] {
            let result = Write
                .execute(
                    &json!({"file_path": path, "content": content}).to_string(),
                    Limits::default(),
                )
                .await;
            assert!(result?.starts_with("File written successfully"));
            assert_eq!(fs::read_to_string(&path).await?, content);
        }

        fs::remove_file(&path).await?;
        fs::remove_dir(&directory).await?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_missing_content_and_propagates_file_errors() {
        assert!(
            Write
                .execute(r#"{"file_path":"unused.txt"}"#, Limits::default())
                .await
                .is_err()
        );
        let arguments = json!({"file_path": "", "content": "Hello"});
        assert!(
            Write
                .execute(&arguments.to_string(), Limits::default())
                .await
                .is_err()
        );
    }
}
