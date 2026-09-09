//! Incremental SSE framing shared by providers. Decode UTF-8 only after a full
//! line arrives: HTTP chunks can split both characters and event delimiters.
use anyhow::Result;

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
enum StreamError {
    #[error("Response stream ended before its completion event")]
    Incomplete,
    #[error("Response stream event exceeds 8 MiB")]
    TooLarge,
    #[error("Response stream contains invalid UTF-8")]
    InvalidUtf8(#[from] std::str::Utf8Error),
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: String,
    cr: bool,
}

impl Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, StreamError> {
        let mut events = Vec::new();
        for &byte in chunk {
            if byte == b'\n' && self.cr {
                self.cr = false;
                continue;
            }
            self.cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                let line = std::str::from_utf8(&self.line)?;
                if line.is_empty() && !self.data.is_empty() {
                    self.data.pop(); // Last data-line separator is not part of the payload.
                    events.push(std::mem::take(&mut self.data));
                } else if let Some(data) = line.strip_prefix("data:") {
                    self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                    self.data.push('\n');
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
            if self.line.len() + self.data.len() > MAX_EVENT_BYTES {
                return Err(StreamError::TooLarge);
            }
        }
        Ok(events)
    }
}

pub(super) async fn read<T>(
    mut response: reqwest::Response,
    mut on_data: impl FnMut(&str) -> Result<Option<T>>,
) -> Result<T> {
    let mut decoder = Decoder::default();
    while let Some(chunk) = response.chunk().await? {
        for data in decoder.push(&chunk)? {
            if let Some(value) = on_data(&data)? {
                return Ok(value);
            }
        }
    }
    Err(StreamError::Incomplete.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_unicode_crlf_comments_and_multiline_data() -> Result<()> {
        let source =
            ": keepalive\r\nevent: message\r\ndata: żółw\r\ndata: second\r\n\r\ndata:[DONE]\n\n";
        for size in 1..=source.len() {
            let mut decoder = Decoder::default();
            let mut events = Vec::new();
            for chunk in source.as_bytes().chunks(size) {
                events.extend(decoder.push(chunk)?);
            }
            assert_eq!(events, ["żółw\nsecond", "[DONE]"]);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_utf8_and_oversized_events() {
        assert!(Decoder::default().push(b"data: \xff\n\n").is_err());
        assert!(
            Decoder::default()
                .push(&vec![b'x'; MAX_EVENT_BYTES + 1])
                .is_err()
        );
    }
}
