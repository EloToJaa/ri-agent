use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::process::Command;

pub(super) struct Search;
pub(super) struct Find;

#[derive(Deserialize)]
struct Arguments {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
}

fn default_path() -> String {
    ".".into()
}

#[derive(Serialize)]
struct CommandOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

async fn run(program: &str, args: &[&str], limits: Limits) -> Result<CommandOutput> {
    let mut child = Command::new(program)
        .args(args)
        .env_remove("RIPGREP_CONFIG_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Failed to start {program}; install it or use the Nix shell"))?;
    let stdout = child.stdout.take().context("Missing stdout")?;
    let stderr = child.stderr.take().context("Missing stderr")?;
    let (status, stdout, stderr) = Box::pin(tokio::time::timeout(limits.command_timeout, async {
        tokio::try_join!(
            async { child.wait().await.context("Waiting for discovery command") },
            crate::limits::read_output(stdout, limits.max_output_bytes),
            crate::limits::read_output(stderr, limits.max_output_bytes),
        )
    }))
    .await
    .with_context(|| format!("{program} timed out"))??;
    // rg uses 1 for an ordinary no-match result.
    if !(status.success() || program == "rg" && status.code() == Some(1)) {
        bail!("{program} failed ({:?}): {stderr}", status.code());
    }
    Ok(CommandOutput {
        stdout,
        stderr,
        exit_code: status.code(),
    })
}

#[derive(Serialize)]
pub struct FileMatches {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// Discover files relative to `path`, respecting fd's default ignore/hidden rules.
/// NUL separators keep spaces and newlines in filenames intact.
pub async fn find_files(pattern: &str, path: &str, limits: Limits) -> Result<FileMatches> {
    let output = run(
        "fd",
        &[
            "--type", "f", "--color", "never", "--print0", "--", pattern, path,
        ],
        limits,
    )
    .await?;
    Ok(parse_files(&output.stdout))
}

fn parse_files(output: &str) -> FileMatches {
    let truncated = output.ends_with("\n[output truncated]");
    // Only retain complete NUL-terminated paths, never a partially retained name.
    let paths = output
        .rsplit_once('\0')
        .map_or_else(Vec::new, |(complete, _)| {
            complete.split('\0').map(str::to_owned).collect()
        });
    FileMatches { paths, truncated }
}

fn parameters(description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": description},
            "path": {"type": "string", "description": "Search root (default: current working directory)"}
        },
        "required": ["pattern"]
    })
}

impl Tool for Search {
    fn name(&self) -> &'static str {
        "Search"
    }
    fn description(&self) -> &'static str {
        "Search file contents with ripgrep (rg). Returns matching paths, line numbers and text. Respects ignore files and skips hidden files by default."
    }
    fn parameters(&self) -> Value {
        parameters("Regular expression to search for")
    }
    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: Arguments =
                serde_json::from_str(arguments).context("Invalid Search arguments")?;
            let output = run(
                "rg",
                &[
                    "--no-config",
                    "--color",
                    "never",
                    "--line-number",
                    "--with-filename",
                    "--no-heading",
                    "--",
                    &args.pattern,
                    &args.path,
                ],
                limits,
            )
            .await?;
            Ok(serde_json::to_string(&output)?)
        })
    }
}

impl Tool for Find {
    fn name(&self) -> &'static str {
        "Find"
    }
    fn description(&self) -> &'static str {
        "Find files by filename regular expression using fd. Use an empty pattern to list files. Respects ignore files and skips hidden files by default."
    }
    fn parameters(&self) -> Value {
        parameters("Filename regular expression; empty string matches all files")
    }
    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: Arguments =
                serde_json::from_str(arguments).context("Invalid Find arguments")?;
            Ok(serde_json::to_string(
                &find_files(&args.pattern, &args.path, limits).await?,
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_paths_only() {
        let files = parse_files("a b\0c\nd\0partial\n[output truncated]");
        assert_eq!(files.paths, ["a b", "c\nd"]);
        assert!(files.truncated);
        assert!(parse_files("").paths.is_empty());
    }

    #[tokio::test]
    async fn bounds_output_and_passes_patterns_without_shell_interpolation() -> Result<()> {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools");
        let limits = Limits {
            max_output_bytes: std::num::NonZeroUsize::MIN,
            ..Limits::default()
        };
        let files = find_files("", root, limits).await?;
        assert!(files.truncated);
        assert!(files.paths.is_empty());
        let result = Search
            .execute(&json!({"pattern": ".", "path": root}).to_string(), limits)
            .await?;
        assert!(result.contains("[output truncated]"));
        for tool in [&Find as &dyn Tool, &Search as &dyn Tool] {
            let result = tool
                .execute(
                    &json!({"pattern": (["--version;", " echo injected"].concat()), "path": root})
                        .to_string(),
                    Limits::default(),
                )
                .await?;
            let result: Value = serde_json::from_str(&result)?;
            if tool.name() == "Find" {
                assert_eq!(result.get("paths"), Some(&json!([])));
            } else {
                assert_eq!(result.get("stdout"), Some(&json!("")));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn finds_files_and_searches_contents() -> Result<()> {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools");
        let files = find_files("^discovery\\.rs$", root, Limits::default()).await?;
        assert_eq!(files.paths.len(), 1);
        assert!(
            files
                .paths
                .first()
                .is_some_and(|path| path.ends_with("discovery.rs"))
        );
        let output = Search
            .execute(
                &json!({"pattern": "fn parses_complete_paths_only", "path": root}).to_string(),
                Limits::default(),
            )
            .await?;
        assert!(output.contains("discovery.rs:"));
        let empty = Search
            .execute(
                &json!({"pattern": "^no-such-content-123456$", "path": root}).to_string(),
                Limits::default(),
            )
            .await?;
        assert_eq!(
            serde_json::from_str::<Value>(&empty)?.get("exit_code"),
            Some(&json!(1))
        );
        assert!(find_files("[", root, Limits::default()).await.is_err());
        assert!(Search.execute("{}", Limits::default()).await.is_err());
        Ok(())
    }
}
