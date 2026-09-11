use super::{Tool, ToolFuture};
use crate::limits::Limits;
use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt};

pub(super) struct Read;

#[derive(Deserialize)]
struct Arguments {
    file_path: PathBuf,
    offset: Option<u64>,
    start_line: Option<usize>,
    max_lines: Option<usize>,
    #[serde(default)]
    include_hash: bool,
}

async fn read_range(
    reader: impl tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin,
    args: &Arguments,
    limits: Limits,
    hash: Option<String>,
) -> anyhow::Result<String> {
    if args.offset.is_some() && args.start_line.is_some() {
        bail!("Use either offset or start_line, not both");
    }
    if args.start_line == Some(0) || args.max_lines == Some(0) {
        bail!("Line numbers and max_lines must be positive");
    }
    let mut reader = tokio::io::BufReader::new(reader);
    reader
        .seek(std::io::SeekFrom::Start(args.offset.unwrap_or(0)))
        .await?;
    let start_line = args.start_line.unwrap_or(1);
    let mut line = 1;
    while line < start_line {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            break;
        }
        let count = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| {
                line += 1;
                index + 1
            });
        reader.consume(count);
    }
    let offset = reader.stream_position().await?;
    let mut bytes = Vec::new();
    reader
        .take((limits.max_output_bytes.get() as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .await?;
    let mut keep = bytes.len().min(limits.max_output_bytes.get());
    if let Some(max_lines) = args.max_lines
        && let Some((index, _)) = bytes
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .nth(max_lines - 1)
    {
        keep = keep.min(index + 1);
    }
    // Do not split a UTF-8 codepoint at a continuation boundary.
    if let Err(error) = std::str::from_utf8(bytes.get(..keep).unwrap_or_default())
        && error.error_len().is_none()
    {
        keep = error.valid_up_to();
    }
    if keep == 0 && !bytes.is_empty() {
        bail!("Output limit is too small for the next UTF-8 character");
    }
    let more = keep < bytes.len();
    bytes.truncate(keep);
    let text = String::from_utf8_lossy(&bytes);
    let content = if args.start_line.is_some() || args.max_lines.is_some() {
        text.split_inclusive('\n')
            .enumerate()
            .map(|(index, text)| format!("{}: {text}", line + index))
            .collect::<Vec<_>>()
            .concat()
    } else {
        text.into_owned()
    };
    Ok(json!({"content":content, "offset":offset, "next_offset": if more { Some(offset + keep as u64) } else { None }, "truncated":more, "sha256":hash}).to_string())
}

impl Tool for Read {
    fn name(&self) -> &'static str {
        "Read"
    }

    fn description(&self) -> &'static str {
        "Read a file. Use start_line (1-based) and max_lines for numbered lines, or offset for bytes. Ranged reads return JSON with content and next_offset; continue with that byte offset. include_hash=true returns the full file sha256 for guarded edits (files up to 16 MiB)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "offset": {"type":"integer", "minimum":0},
                "start_line": {"type":"integer", "minimum":1},
                "max_lines": {"type":"integer", "minimum":1},
                "include_hash": {"type":"boolean"},
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
            if arguments.include_hash {
                let snapshot =
                    super::files::Snapshot::read(arguments.file_path.clone(), false, None).await?;
                let content = snapshot.content.context("Missing file contents")?;
                let hash = super::files::hash(content.as_bytes());
                return read_range(
                    std::io::Cursor::new(content),
                    &arguments,
                    limits,
                    Some(hash),
                )
                .await;
            }
            if !fs::metadata(&arguments.file_path).await?.is_file() {
                bail!("Read requires a regular file");
            }
            let file = fs::File::open(&arguments.file_path)
                .await
                .with_context(|| format!("Failed to read {}", arguments.file_path.display()))?;
            if !file.metadata().await?.is_file() {
                bail!("Read requires a regular file");
            }
            if arguments.offset.is_some()
                || arguments.start_line.is_some()
                || arguments.max_lines.is_some()
            {
                return read_range(file, &arguments, limits, None).await;
            }
            let range: Value =
                serde_json::from_str(&read_range(file, &arguments, limits, None).await?)?;
            let mut content = range
                .get("content")
                .and_then(Value::as_str)
                .context("Missing read contents")?
                .to_owned();
            if let Some(offset) = range.get("next_offset").and_then(Value::as_u64) {
                use std::fmt::Write as _;
                let _ = write!(
                    content,
                    "\n[output truncated; continue with Read offset={offset}]"
                );
            }
            Ok(content)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ranges_number_lines_and_continue_without_splitting_unicode() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("text");
        fs::write(&path, "one\nżółw\nlast\n").await?;
        let output: Value = serde_json::from_str(
            &Read
                .execute(
                    &json!({"file_path":path,"start_line":2,"max_lines":1,"include_hash":true})
                        .to_string(),
                    Limits::default(),
                )
                .await?,
        )?;
        assert_eq!(output.get("content"), Some(&json!("2: żółw\n")));
        assert_eq!(
            output.get("sha256"),
            Some(&json!(super::super::files::hash(
                "one\nżółw\nlast\n".as_bytes()
            )))
        );
        let offset = output
            .get("next_offset")
            .and_then(Value::as_u64)
            .context("continuation")?;
        let next: Value = serde_json::from_str(
            &Read
                .execute(
                    &json!({"file_path":path,"offset":offset}).to_string(),
                    Limits::default(),
                )
                .await?,
        )?;
        assert_eq!(next.get("content"), Some(&json!("last\n")));
        fs::write(&path, "ażb").await?;
        let limits = Limits {
            max_output_bytes: std::num::NonZeroUsize::MIN.saturating_add(1),
            ..Limits::default()
        };
        let first: Value = serde_json::from_str(
            &Read
                .execute(&json!({"file_path":path,"offset":0}).to_string(), limits)
                .await?,
        )?;
        assert_eq!(first.get("content"), Some(&json!("a")));
        assert_eq!(first.get("next_offset"), Some(&json!(1)));
        Ok(())
    }
}
