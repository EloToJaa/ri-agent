use anyhow::Result;
use std::num::NonZeroUsize;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAX_TURNS: NonZeroUsize = NonZeroUsize::MIN.saturating_add(19);

#[derive(Clone, Copy)]
pub struct Limits {
    pub command_timeout: std::time::Duration,
    pub max_output_bytes: NonZeroUsize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            command_timeout: std::time::Duration::from_mins(1),
            max_output_bytes: NonZeroUsize::MIN.saturating_add(32 * 1024 - 1),
        }
    }
}

// Drain command pipes even after the cap, so children cannot block on a full pipe.
pub(crate) async fn read_output(
    mut input: impl AsyncRead + Unpin,
    cap: NonZeroUsize,
) -> Result<String> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(cap.get().saturating_sub(output.len()));
        output.extend(buffer.iter().take(keep));
        truncated |= keep < count;
    }
    let mut text = String::from_utf8_lossy(&output).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caps_output_and_marks_truncation() -> Result<()> {
        let cap = NonZeroUsize::MIN.saturating_add(2);
        assert_eq!(
            read_output(b"abcdef".as_slice(), cap).await?,
            "abc\n[output truncated]"
        );
        assert_eq!(read_output(b"abc".as_slice(), cap).await?, "abc");
        Ok(())
    }
}
