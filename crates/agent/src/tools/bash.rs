use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

pub(super) struct Bash;

#[derive(Deserialize)]
struct Arguments {
    command: Option<String>,
    command_id: Option<String>,
    #[serde(default)]
    action: Action,
    yield_time_ms: Option<u64>,
    timeout_ms: Option<u64>,
    cwd: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    #[default]
    Start,
    Poll,
    Cancel,
}

pub(super) async fn execute_managed(
    arguments: &str,
    mut limits: Limits,
    commands: &super::commands::Commands,
    output: &crate::events::Output,
    cancellation: &crate::cancellation::Cancellation,
) -> anyhow::Result<String> {
    let args: Arguments = serde_json::from_str(arguments).context("Invalid Bash arguments")?;
    if let Some(timeout) = args.timeout_ms {
        if timeout == 0 || timeout > 43_200_000 {
            bail!("timeout_ms must be between 1 and 43200000");
        }
        limits.command_timeout = Duration::from_millis(timeout);
    }
    let wait = args.yield_time_ms.map_or(
        limits.command_timeout + Duration::from_secs(2),
        Duration::from_millis,
    );
    if args.yield_time_ms.is_some_and(|value| value > 60_000) {
        bail!("yield_time_ms must be at most 60000; use poll for longer commands");
    }
    let (id, cancel) = match args.action {
        Action::Start => {
            if args.command_id.is_some() {
                bail!("start does not accept command_id");
            }
            (
                commands.start(
                    args.command.as_deref().context("Missing Bash command")?,
                    args.cwd.as_deref(),
                    limits,
                    output,
                )?,
                false,
            )
        }
        Action::Poll | Action::Cancel => {
            if args.command.is_some() || args.cwd.is_some() || args.timeout_ms.is_some() {
                bail!("poll/cancel only accept command_id and yield_time_ms");
            }
            (
                args.command_id.context("Missing command_id")?,
                matches!(args.action, Action::Cancel),
            )
        }
    };
    serde_json::to_string(&commands.poll(&id, wait, cancel, cancellation).await?)
        .context("Failed to serialize Bash output")
}

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Run Bash with optional cwd/timeout_ms. Set yield_time_ms (0-60000) to return a command_id while it runs; action=poll retrieves new output and action=cancel stops it. Handles belong to this session/process. Each start uses a new shell without interactive input."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type":"string", "enum":["start","poll","cancel"]},
                "command_id": {"type":"string"},
                "yield_time_ms": {"type":"integer", "minimum":0,"maximum":60000},
                "timeout_ms": {"type":"integer", "minimum":1,"maximum":43_200_000},
                "cwd": {"type":"string"},
                "command": {
                    "type": "string",
                    "description": "The Bash command to execute"
                }
            }
        })
    }

    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move {
            execute_managed(
                arguments,
                limits,
                &super::commands::Commands::default(),
                &crate::events::Output::default(),
                &crate::cancellation::Cancellation::default(),
            )
            .await
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
        assert_eq!(output.get("stdout"), Some(&json!("hello")));
        assert_eq!(output.get("exit_code"), Some(&json!(0)));
        assert_eq!(output.get("running"), Some(&json!(false)));
        Ok(())
    }

    #[tokio::test]
    async fn returns_stderr_and_nonzero_status_as_a_tool_result() -> anyhow::Result<()> {
        let output = execute("printf 'failed' >&2; exit 7").await?;
        assert_eq!(output.get("stderr"), Some(&json!("failed")));
        assert_eq!(output.get("exit_code"), Some(&json!(7)));
        assert_eq!(output.get("success"), Some(&json!(false)));
        Ok(())
    }

    #[tokio::test]
    async fn times_out_and_caps_command_output() -> anyhow::Result<()> {
        let limits = Limits {
            command_timeout: std::time::Duration::from_millis(50),
            max_output_bytes: std::num::NonZeroUsize::MIN.saturating_add(2),
            read_only: false,
            ask: false,
        };
        let result = Bash
            .execute(r#"{"command":"exec sleep 5"}"#, limits)
            .await?;
        assert_eq!(
            serde_json::from_str::<Value>(&result)?.get("timed_out"),
            Some(&json!(true))
        );
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
