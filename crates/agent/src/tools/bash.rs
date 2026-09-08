use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::process::Command;

pub(super) struct Bash;

#[derive(Deserialize)]
struct Arguments {
    command: String,
}

#[derive(Serialize)]
struct BashOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    success: bool,
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Execute a Bash command in the current working directory and return stdout, stderr, and exit status. Each call starts a new shell with no interactive input."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The Bash command to execute"
                }
            },
            "required": ["command"]
        })
    }

    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments: Arguments =
                serde_json::from_str(arguments).context("Invalid Bash arguments")?;
            let mut child = Command::new("bash")
                .arg("-c")
                .arg(arguments.command)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("Failed to execute Bash command")?;
            let stdout = child.stdout.take().context("Missing Bash stdout")?;
            let stderr = child.stderr.take().context("Missing Bash stderr")?;
            let (status, stdout, stderr) =
                Box::pin(tokio::time::timeout(limits.command_timeout, async {
                    tokio::try_join!(
                        async { child.wait().await.context("Failed to wait for Bash") },
                        crate::limits::read_output(stdout, limits.max_output_bytes),
                        crate::limits::read_output(stderr, limits.max_output_bytes),
                    )
                }))
                .await
                .context("Bash command timed out")??;

            // A failed command is a tool result the model can inspect and act on.
            serde_json::to_string(&BashOutput {
                stdout,
                stderr,
                exit_code: status.code(),
                success: status.success(),
            })
            .context("Failed to serialize Bash output")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn execute(command: &str) -> anyhow::Result<Value> {
        Ok(serde_json::from_str(
            &Bash
                .execute(&json!({"command": command}).to_string(), Limits::default())
                .await?,
        )?)
    }

    #[tokio::test]
    async fn executes_shell_syntax_and_captures_stdout() -> anyhow::Result<()> {
        let output = execute(
            "value=hello; printf '%s' \"$value\" | while read -r -n 1 char; do printf '%s' \"$char\"; done",
        ).await?;
        assert_eq!(
            output,
            json!({"stdout": "hello", "stderr": "", "exit_code": 0, "success": true})
        );
        Ok(())
    }

    #[tokio::test]
    async fn returns_stderr_and_nonzero_status_as_a_tool_result() -> anyhow::Result<()> {
        let output = execute("printf 'failed' >&2; exit 7").await?;
        assert_eq!(
            output,
            json!({"stdout": "", "stderr": "failed", "exit_code": 7, "success": false})
        );
        Ok(())
    }

    #[tokio::test]
    async fn times_out_and_caps_command_output() -> anyhow::Result<()> {
        let limits = Limits {
            command_timeout: std::time::Duration::from_millis(50),
            max_output_bytes: std::num::NonZeroUsize::MIN.saturating_add(2),
        };
        let error = Bash
            .execute(r#"{"command":"exec sleep 5"}"#, limits)
            .await
            .err()
            .context("Expected a timeout")?;
        assert!(error.to_string().contains("timed out"));
        let result = Bash
            .execute(
                r#"{"command":"printf abcdef; printf ghijkl >&2"}"#,
                Limits {
                    command_timeout: std::time::Duration::from_secs(5),
                    ..limits
                },
            )
            .await?;
        let result: Value = serde_json::from_str(&result)?;
        assert_eq!(
            result.get("stdout"),
            Some(&json!("abc\n[output truncated]"))
        );
        assert_eq!(
            result.get("stderr"),
            Some(&json!("ghi\n[output truncated]"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_missing_command() {
        assert!(Bash.execute("{}", Limits::default()).await.is_err());
    }
}
