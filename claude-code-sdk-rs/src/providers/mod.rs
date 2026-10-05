//! Provider adapters behind the agent contract ([`crate::agent`]).
//!
//! Each adapter implements [`AgentProvider`](crate::agent::AgentProvider) and
//! [`AgentSession`](crate::agent::AgentSession) for one family of provider. An
//! adapter never builds a process by itself: the single launcher is
//! [`crate::transport::spawn::isolated_command`], and a guard test
//! (`tests/providers_spawn_guard.rs`) refuses any other spawn under this
//! directory.

#[cfg(feature = "provider-acp")]
pub mod acp;
pub mod claude_code;
#[cfg(feature = "provider-codex")]
pub mod codex;
#[cfg(feature = "provider-native")]
pub mod native;

/// Reading the lines of an adapter's child process, with a bound.
///
/// `tokio`'s `lines()` grows its buffer until it meets a line feed: a process
/// that prints a huge line, or one that never prints a line feed, makes the host
/// allocate without limit. [`lines::BoundedLines`] keeps at most
/// [`lines::MAX_LINE_BYTES`] of a line, **before** allocating: the moment a line
/// would pass the bound it reports [`lines::Line::TooLong`] (typed as `protocol`
/// by [`lines::too_long_error`]), drops what it held and discards the rest of
/// that line without storing it.
#[cfg(any(
    feature = "provider-native",
    feature = "provider-codex",
    feature = "provider-acp"
))]
pub(crate) mod lines {
    use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

    use crate::agent::ProviderError;

    /// Longest line accepted from a child process: 8 MiB (a large tool output or
    /// model message fits; a flood does not).
    pub(crate) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

    /// One line of the process, or the news that it was too long to be kept.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Line {
        /// A complete line, without its line feed (nor a carriage return).
        Text(String),
        /// The line passed the bound: nothing of it was kept.
        TooLong,
    }

    /// The error a transport reports for [`Line::TooLong`]: `protocol`, no content.
    pub(crate) fn too_long_error() -> ProviderError {
        ProviderError::protocol(format!(
            "a line from the process passed {MAX_LINE_BYTES} bytes and was dropped"
        ))
    }

    /// A line reader that never holds more than `max` bytes of a line.
    pub(crate) struct BoundedLines<R> {
        reader: BufReader<R>,
        line: Vec<u8>,
        max: usize,
        /// The rest of an over-long line is being thrown away.
        discarding: bool,
    }

    impl<R: AsyncRead + Unpin> BoundedLines<R> {
        /// Reads `reader` with the standard bound.
        pub(crate) fn new(reader: R) -> Self {
            Self::with_limit(reader, MAX_LINE_BYTES)
        }

        /// Reads `reader`, keeping at most `max` bytes of a line.
        pub(crate) fn with_limit(reader: R, max: usize) -> Self {
            Self {
                reader: BufReader::new(reader),
                line: Vec::new(),
                max,
                discarding: false,
            }
        }

        /// Bytes of the current line held right now (never more than the bound).
        #[cfg(test)]
        pub(crate) fn held(&self) -> usize {
            self.line.len()
        }

        /// The next line; `None` at the end of the stream.
        pub(crate) async fn next_line(&mut self) -> std::io::Result<Option<Line>> {
            loop {
                let available = self.reader.fill_buf().await?;
                if available.is_empty() {
                    // End of stream: a last line without a line feed still counts.
                    if std::mem::take(&mut self.discarding) || self.line.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(self.finish_line()));
                }
                let (length, ended) = match available.iter().position(|byte| *byte == b'\n') {
                    Some(index) => (index, true),
                    None => (available.len(), false),
                };
                let consumed = length + usize::from(ended);
                if self.discarding {
                    self.reader.consume(consumed);
                    self.discarding = !ended;
                    continue;
                }
                if self.line.len() + length > self.max {
                    // Checked before copying: nothing past the bound is ever held.
                    self.line = Vec::new();
                    self.discarding = !ended;
                    self.reader.consume(consumed);
                    return Ok(Some(Line::TooLong));
                }
                self.line.extend_from_slice(&available[..length]);
                self.reader.consume(consumed);
                if ended {
                    return Ok(Some(self.finish_line()));
                }
            }
        }

        fn finish_line(&mut self) -> Line {
            let mut bytes = std::mem::take(&mut self.line);
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            Line::Text(String::from_utf8_lossy(&bytes).into_owned())
        }
    }

    #[cfg(test)]
    mod tests {
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll};

        use tokio::io::ReadBuf;

        use super::*;

        /// An endless stream of `a`, with no line feed, that counts what was read.
        struct Endless(Arc<AtomicUsize>);

        impl AsyncRead for Endless {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                let room = buf.remaining();
                buf.put_slice(&vec![b'a'; room]);
                self.0.fetch_add(room, Ordering::SeqCst);
                Poll::Ready(Ok(()))
            }
        }

        #[tokio::test]
        async fn a_line_without_end_is_cut_at_the_bound_before_it_is_stored() {
            let read = Arc::new(AtomicUsize::new(0));
            let mut lines = BoundedLines::with_limit(Endless(Arc::clone(&read)), 1024);
            let first = lines.next_line().await.unwrap();
            assert_eq!(first, Some(Line::TooLong));
            assert!(lines.held() <= 1024, "held {}", lines.held());
            // The reader stopped right after the bound: one buffer past it at most,
            // not the gigabytes an endless stream would have cost.
            let consumed = read.load(Ordering::SeqCst);
            assert!(consumed <= 1024 + 2 * 8 * 1024, "read {consumed} bytes");
            assert_eq!(too_long_error().kind(), "protocol");
        }

        #[tokio::test]
        async fn an_over_long_line_is_dropped_and_the_next_ones_are_read() {
            let text = format!("{}\nok\r\nlast", "x".repeat(50));
            let mut lines = BoundedLines::with_limit(text.as_bytes(), 10);
            assert_eq!(lines.next_line().await.unwrap(), Some(Line::TooLong));
            assert_eq!(
                lines.next_line().await.unwrap(),
                Some(Line::Text("ok".into()))
            );
            // A last line without a line feed is still a line.
            assert_eq!(
                lines.next_line().await.unwrap(),
                Some(Line::Text("last".into()))
            );
            assert_eq!(lines.next_line().await.unwrap(), None);
        }

        #[tokio::test]
        async fn a_line_at_the_bound_is_kept_and_an_unfinished_over_long_one_ends_quietly() {
            let exact = "y".repeat(10);
            let input = format!("{exact}\n{}", "z".repeat(11));
            let mut lines = BoundedLines::with_limit(input.as_bytes(), 10);
            assert_eq!(lines.next_line().await.unwrap(), Some(Line::Text(exact)));
            assert_eq!(lines.next_line().await.unwrap(), Some(Line::TooLong));
            assert_eq!(lines.next_line().await.unwrap(), None);
        }
    }
}
