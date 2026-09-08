use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::fs;

pub(super) struct Write;

#[derive(Deserialize)]
struct Arguments {
    file_path: PathBuf,
    content: String,
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

    fn execute<'a>(&'a self, arguments: &'a str, _limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments: Arguments =
                serde_json::from_str(arguments).context("Invalid Write arguments")?;
            fs::write(&arguments.file_path, arguments.content)
                .await
                .with_context(|| format!("Failed to write {}", arguments.file_path.display()))?;
            Ok("File written successfully".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn creates_and_overwrites_a_file() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("write-tool-{}-{unique}", std::process::id()));
        fs::create_dir(&directory).await.unwrap();
        let path = directory.join("output.txt");

        for content in ["Hello, world!", "Short", ""] {
            let result = Write
                .execute(
                    &json!({"file_path": path, "content": content}).to_string(),
                    Limits::default(),
                )
                .await;
            assert_eq!(result.unwrap(), "File written successfully");
            assert_eq!(fs::read_to_string(&path).await.unwrap(), content);
        }

        fs::remove_file(&path).await.unwrap();
        fs::remove_dir(&directory).await.unwrap();
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
