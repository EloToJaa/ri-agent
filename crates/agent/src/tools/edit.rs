use super::{Tool, ToolFuture};
use crate::limits::{Limits, read_output};
use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fmt::Write as _, path::PathBuf};
#[cfg(test)]
use tokio::fs;

pub(super) struct Edit;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    file_path: PathBuf,
    old_string: String,
    new_string: String,
    expected_sha256: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum EditError {
    #[error("old_string must not be empty; include surrounding text to identify the edit")]
    EmptyMatch,
    #[error("old_string was not found; read the file again before editing")]
    MissingMatch,
    #[error("old_string matches more than once; include more surrounding text")]
    AmbiguousMatch,
    #[error("old_string and new_string are identical")]
    Unchanged,
}

fn replace(source: &str, old: &str, new: &str) -> Result<String, EditError> {
    if old.is_empty() {
        return Err(EditError::EmptyMatch);
    }
    let start = source.find(old).ok_or(EditError::MissingMatch)?;
    // Check overlapping occurrences too (for example, "aa" in "aaa").
    let next = start + old.chars().next().map_or(0, char::len_utf8);
    if source.get(next..).is_some_and(|tail| tail.contains(old)) {
        return Err(EditError::AmbiguousMatch);
    }
    if old == new {
        return Err(EditError::Unchanged);
    }
    Ok(source.replacen(old, new, 1))
}

// A single replacement needs one hunk. Preserve line endings and explicitly mark
// missing final newlines so the displayed change is not misleading.
pub(super) fn diff(path: &str, before: &str, after: &str) -> String {
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old
        .iter()
        .skip(prefix)
        .rev()
        .zip(new.iter().skip(prefix).rev())
        .take_while(|(a, b)| a == b)
        .count();
    let context_before = prefix.min(3);
    let context_after = suffix.min(3);
    let start = prefix - context_before;
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    let old_count = old_end + context_after - start;
    let new_count = new_end + context_after - start;
    let mut result = format!("--- {path}\n+++ {path}\n");
    let _ = writeln!(
        result,
        "@@ -{},{} +{},{} @@",
        start + usize::from(old_count > 0),
        old_count,
        start + usize::from(new_count > 0),
        new_count
    );
    for (marker, lines) in [
        (' ', old.get(start..prefix).unwrap_or_default()),
        ('-', old.get(prefix..old_end).unwrap_or_default()),
        ('+', new.get(prefix..new_end).unwrap_or_default()),
        (
            ' ',
            old.get(old_end..old_end + context_after)
                .unwrap_or_default(),
        ),
    ] {
        for line in lines {
            result.push(marker);
            result.push_str(line);
            if !line.ends_with('\n') {
                result.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    result
}

pub(super) async fn prepare(
    arguments: &str,
    limits: Limits,
) -> anyhow::Result<super::files::Mutation> {
    let args: Arguments = serde_json::from_str(arguments).context("Invalid Edit arguments")?;
    let snapshot = super::files::Snapshot::read(
        args.file_path.clone(),
        false,
        args.expected_sha256.as_deref(),
    )
    .await?;
    let source = snapshot.content.as_deref().context("Missing edit source")?;
    let edited = replace(source, &args.old_string, &args.new_string)?;
    let patch = diff(&args.file_path.to_string_lossy(), source, &edited);
    let patch = read_output(patch.as_bytes(), limits.max_output_bytes).await?;
    snapshot.prepare(edited, format!("File edited successfully\n{patch}"))
}

impl Tool for Edit {
    fn name(&self) -> &'static str {
        "Edit"
    }

    fn description(&self) -> &'static str {
        "Replace one unique exact string in an existing UTF-8 file. Read the file first. Include enough surrounding text for a unique match. Returns a unified diff."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {
            "file_path": {"type": "string", "description": "Path to the existing file"},
            "expected_sha256": {"type":"string", "description":"File hash returned by Read(include_hash=true); rejects stale edits"},
            "old_string": {"type": "string", "description": "Nonempty exact text occurring once"},
            "new_string": {"type": "string", "description": "Replacement text; empty deletes the match"}
        }, "required": ["file_path", "old_string", "new_string"], "additionalProperties": false})
    }

    fn execute<'a>(&'a self, arguments: &'a str, limits: Limits) -> ToolFuture<'a> {
        Box::pin(async move { prepare(arguments, limits).await?.apply().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_ambiguous_overlapping_and_empty_matches() {
        for (source, old, new) in [
            ("abc", "", "x"),
            ("abc", "z", "x"),
            ("abc abc", "abc", "x"),
            ("aaa", "aa", "x"),
            ("ééé", "éé", "x"),
            ("abc", "abc", "abc"),
        ] {
            assert!(replace(source, old, new).is_err());
        }
    }

    #[test]
    fn diff_preserves_context_and_missing_newlines() {
        assert_eq!(
            diff("file", "one\ntwo\nthree", "one\nnew\nthree"),
            "--- file\n+++ file\n@@ -1,3 +1,3 @@\n one\n-two\n+new\n three\n\\ No newline at end of file\n"
        );
        assert_eq!(
            diff("file", "a\n", ""),
            "--- file\n+++ file\n@@ -1,1 +0,0 @@\n-a\n"
        );
    }

    #[tokio::test]
    async fn edits_unicode_and_deletes_while_failed_edits_preserve_the_file() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("file.txt");
        fs::write(&path, "first\r\nżółw\r\nlast\r\n").await?;
        let call = |old: &str, new: &str| {
            json!({"file_path": path, "old_string": old, "new_string": new}).to_string()
        };
        let result = Edit
            .execute(&call("żółw", "kot"), Limits::default())
            .await?;
        assert!(result.contains("-żółw\r\n+kot\r\n"));
        let expected = "first\r\nkot\r\nlast\r\n";
        assert_eq!(fs::read_to_string(&path).await?, expected);
        assert!(
            Edit.execute(&call("missing", "x"), Limits::default())
                .await
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).await?, expected);
        Edit.execute(&call("kot\r\n", ""), Limits::default())
            .await?;
        assert_eq!(fs::read_to_string(&path).await?, "first\r\nlast\r\n");
        Ok(())
    }

    #[tokio::test]
    async fn caps_diff_without_truncating_the_edit_and_rejects_nonexistent_files()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("file");
        let args = json!({"file_path": path, "old_string": "a", "new_string": "b".repeat(100)})
            .to_string();
        assert!(Edit.execute(&args, Limits::default()).await.is_err());
        assert!(!path.exists());
        fs::write(&path, "a").await?;
        let limits = Limits {
            max_output_bytes: std::num::NonZeroUsize::MIN.saturating_add(15),
            ..Limits::default()
        };
        assert!(
            Edit.execute(&args, limits)
                .await?
                .ends_with("[output truncated]")
        );
        assert_eq!(fs::read_to_string(path).await?, "b".repeat(100));
        Ok(())
    }
}
