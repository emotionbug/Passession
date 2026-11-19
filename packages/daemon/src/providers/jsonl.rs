use std::io;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

// Allow large tool results (including encoded file contents) and history
// replies; OMP also allows 64 MiB for a reassembled v2 message.
pub(super) const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_RETAINED_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(super) enum LineError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("JSONL stdout frame exceeds {limit} bytes (including newline)")]
    Oversized { limit: usize },
    #[error("JSONL stdout ended with an unterminated frame")]
    Unterminated,
}

/// A cancel-safe JSONL reader. Partial bytes stay in `line` when a select
/// branch is canceled. Oversized or truncated frames are terminal errors.
pub(super) struct LineReader<R> {
    input: BufReader<R>,
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub(super) fn new(input: R) -> Self {
        Self {
            input: BufReader::new(input),
            line: Vec::new(),
        }
    }

    pub(super) async fn next_line(&mut self) -> Result<bool, LineError> {
        self.next_line_with_limit(MAX_FRAME_BYTES).await
    }

    async fn next_line_with_limit(&mut self, limit: usize) -> Result<bool, LineError> {
        if self.line.last() == Some(&b'\n') {
            self.clear_line();
        }
        loop {
            let available = self.input.fill_buf().await?;
            if available.is_empty() {
                return if self.line.is_empty() {
                    Ok(false)
                } else {
                    Err(LineError::Unterminated)
                };
            }
            let remaining = limit - self.line.len();
            let newline = available[..available.len().min(remaining)]
                .iter()
                .position(|byte| *byte == b'\n');
            let count = match newline {
                Some(index) => index + 1,
                None if available.len() < remaining => available.len(),
                None => return Err(LineError::Oversized { limit }),
            };
            let len = self.line.len() + count;
            if len > self.line.capacity() {
                let capacity = len.max(self.line.capacity().saturating_mul(2)).min(limit);
                self.line.reserve_exact(capacity - self.line.len());
            }
            self.line.extend_from_slice(&available[..count]);
            self.input.consume(count);
            if newline.is_some() {
                return Ok(true);
            }
        }
    }

    pub(super) fn line(&self) -> &[u8] {
        &self.line
    }

    pub(super) fn clear_line(&mut self) {
        if self.line.capacity() > MAX_RETAINED_BYTES {
            self.line = Vec::new();
        } else {
            self.line.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn rejects_oversized_frame_without_waiting_for_newline_or_eof() {
        let (mut writer, input) = tokio::io::duplex(4);
        let mut lines = LineReader::new(input);
        writer.write_all(b"xxxx").await.unwrap();
        let mut line = std::pin::pin!(lines.next_line_with_limit(4));
        assert!(matches!(
            futures_util::poll!(&mut line),
            Poll::Ready(Err(error)) if error.to_string().contains("exceeds 4 bytes")
        ));
    }

    #[tokio::test]
    async fn exact_limit_and_partial_eof_are_distinct() {
        let mut lines = LineReader::new(&b"123\nabc"[..]);
        assert!(lines.next_line_with_limit(4).await.unwrap());
        assert_eq!(lines.line(), b"123\n");
        let error = lines.next_line_with_limit(4).await.unwrap_err();
        assert!(error.to_string().contains("unterminated frame"));
    }

    #[tokio::test]
    async fn canceled_read_preserves_partial_frame() {
        let (mut writer, input) = tokio::io::duplex(4);
        let mut lines = LineReader::new(input);
        writer.write_all(b"ab").await.unwrap();
        {
            let mut line = std::pin::pin!(lines.next_line());
            assert!(futures_util::poll!(&mut line).is_pending());
        }
        writer.write_all(b"c\n").await.unwrap();
        assert!(lines.next_line().await.unwrap());
        assert_eq!(lines.line(), b"abc\n");
    }

    #[tokio::test]
    async fn large_json_line_passes_and_its_allocation_is_released() {
        let size = 2 * MAX_RETAINED_BYTES;
        let input = format!("{{\"file\":\"{}\"}}\n{{}}\n", "x".repeat(size));
        let mut lines = LineReader::new(input.as_bytes());
        assert!(lines.next_line().await.unwrap());
        let frame: serde_json::Value = serde_json::from_slice(lines.line()).unwrap();
        assert_eq!(frame["file"].as_str().unwrap().len(), size);
        assert!(lines.next_line().await.unwrap());
        assert_eq!(lines.line(), b"{}\n");
        assert!(lines.line.capacity() <= MAX_RETAINED_BYTES);
        assert!(!lines.next_line().await.unwrap());
    }
}
