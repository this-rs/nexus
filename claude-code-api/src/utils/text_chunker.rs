//! Text chunking utilities for simulating streaming output
//!
//! Since Claude CLI returns complete messages, we need to chunk them
//! to provide a better streaming experience.

#![allow(dead_code)] // Public API - may not be used internally

use futures::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Interval, interval};

/// Configuration for text chunking
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Size of each chunk in characters
    pub chunk_size: usize,
    /// Delay between chunks in milliseconds
    pub chunk_delay_ms: u64,
    /// Whether to split at word boundaries
    pub word_boundary: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            chunk_size: 20,      // ~3-5 words per chunk
            chunk_delay_ms: 50,  // 50ms between chunks for smooth streaming
            word_boundary: true, // Split at word boundaries for natural flow
        }
    }
}

/// A stream that chunks text into smaller pieces with delays
pub struct TextChunker {
    text: String,
    position: usize,
    config: ChunkConfig,
    interval: Interval,
}

impl TextChunker {
    /// Create a new text chunker
    ///
    /// `chunk_delay_ms: 0` is clamped to one millisecond: `tokio::time::interval`
    /// panics on a zero period, so the obvious way of asking for "no delay" used
    /// to take the whole task down.
    pub fn new(text: String, config: ChunkConfig) -> Self {
        let interval = interval(Duration::from_millis(config.chunk_delay_ms.max(1)));
        Self {
            text,
            position: 0,
            config,
            interval,
        }
    }

    /// Get the next chunk of text
    fn next_chunk(&mut self) -> Option<String> {
        if self.position >= self.text.len() {
            return None;
        }

        let remaining = &self.text[self.position..];
        let end = chunk_end(remaining, &self.config);

        let chunk = remaining[..end].to_string();
        self.position += end;
        Some(chunk)
    }
}

/// Byte length of the first `max_chars` characters of `text`, or the whole string
/// when it holds fewer.
///
/// `ChunkConfig::chunk_size` is documented as a count of *characters*; this is
/// what turns it into the byte offset a `str` slice needs.
fn char_prefix_len(text: &str, max_chars: usize) -> usize {
    text.char_indices()
        .nth(max_chars)
        .map_or(text.len(), |(idx, _)| idx)
}

/// Where to cut `remaining` for the next chunk, as a byte offset.
///
/// The single place both chunkers compute a cut, so they cannot disagree. Two
/// properties the callers rely on, and which earlier byte-arithmetic broke:
///
/// * the offset is always on a **character boundary**, so `&remaining[..offset]`
///   never panics — `chunk_size` used to be compared against `remaining.len()`
///   (a byte count) and sliced straight away, which aborted the streaming task on
///   any multi-byte text (accented French, Japanese, emoji);
/// * the offset is always **greater than zero** for non-empty input, so a caller
///   that advances by it makes progress — `chunk_size: 0` used to yield endless
///   zero-length chunks, i.e. an unbounded loop in [`split_text_into_chunks`].
///
/// Word-boundary search stays byte-indexed on purpose: it only ever looks for
/// `' '`, a one-byte character that cannot occur inside a multi-byte sequence, so
/// `index + 1` is a character boundary.
fn chunk_end(remaining: &str, config: &ChunkConfig) -> usize {
    // `max(1)`: at least one character per chunk, whatever the configuration.
    let mut end = char_prefix_len(remaining, config.chunk_size.max(1));

    // If word_boundary is enabled, try to break at word boundaries
    if config.word_boundary && end < remaining.len() {
        // Look for the last space within the chunk
        if let Some(last_space) = remaining[..end].rfind(' ') {
            if last_space > 0 {
                end = last_space + 1; // Include the space
            }
        } else if let Some(next_space) = remaining[end..].find(' ') {
            // No space found in chunk, look forward for the next space
            end += next_space + 1;
        }
    }

    debug_assert!(
        end > 0 && remaining.is_char_boundary(end),
        "chunk_end must advance and land on a char boundary"
    );
    end
}

impl Stream for TextChunker {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Wait for the interval
        match self.interval.poll_tick(cx) {
            Poll::Ready(_) => {
                // Get next chunk
                Poll::Ready(self.next_chunk())
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Create a chunked stream from text
pub fn chunk_text(text: String, config: Option<ChunkConfig>) -> impl Stream<Item = String> {
    TextChunker::new(text, config.unwrap_or_default())
}

/// Split text into chunks for array processing
pub fn split_text_into_chunks(text: &str, config: &ChunkConfig) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut position = 0;

    while position < text.len() {
        let remaining = &text[position..];
        let end = chunk_end(remaining, config);

        chunks.push(remaining[..end].to_string());
        position += end;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::StreamExt;

    // ── split_text_into_chunks: basic (no word boundary) ──

    #[test]
    fn test_split_text_basic() {
        let text = "Hello world, this is a test message.";
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: false,
        };

        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks[0], "Hello worl");
        assert_eq!(chunks[1], "d, this is");
    }

    #[test]
    fn test_split_basic_reassembles_to_original() {
        let text = "Hello world, this is a test message.";
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        let chunks = split_text_into_chunks(text, &config);
        let reassembled: String = chunks.into_iter().collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn test_split_basic_exact_chunk_size() {
        // Text length is exactly chunk_size
        let text = "0123456789";
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "0123456789");
    }

    #[test]
    fn test_split_basic_smaller_than_chunk() {
        let text = "Hi";
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hi");
    }

    #[test]
    fn test_split_basic_chunk_size_one() {
        let text = "abc";
        let config = ChunkConfig {
            chunk_size: 1,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks, vec!["a", "b", "c"]);
    }

    // ── split_text_into_chunks: word boundary mode ──

    #[test]
    fn test_split_text_word_boundary() {
        let text = "Hello world, this is a test message.";
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: true,
        };

        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks[0], "Hello ");
        assert_eq!(chunks[1], "world, ");
    }

    #[test]
    fn test_split_word_boundary_reassembles() {
        let text = "Hello world, this is a test message.";
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks(text, &config);
        let reassembled: String = chunks.into_iter().collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn test_split_word_boundary_long_word_no_space_in_chunk() {
        // A word longer than chunk_size: should look forward for next space
        let text = "superlongword next";
        let config = ChunkConfig {
            chunk_size: 5,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks(text, &config);
        // No space in first 5 chars, so it looks forward and finds space at index 13
        assert_eq!(chunks[0], "superlongword ");
        assert_eq!(chunks[1], "next");
    }

    #[test]
    fn test_split_word_boundary_single_long_word() {
        // A single word with no space at all — should return entire text as one chunk
        let text = "abcdefghijklmnopqrstuvwxyz";
        let config = ChunkConfig {
            chunk_size: 5,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks(text, &config);
        // No space found at all, so chunk_end stays at 5 (no space backward or forward)
        // Actually the forward search finds nothing, so chunk_end remains at 5
        assert_eq!(chunks.len(), 6); // 26 chars / 5 = 5 full + 1 partial
        let reassembled: String = chunks.into_iter().collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn test_split_word_boundary_space_at_start() {
        // rfind(' ') returns index 0 which is NOT > 0, so it won't use that boundary
        // BUT since rfind returned Some, we are in the `if let Some` arm, NOT the `else if` arm.
        // So chunk_end stays at 5 (the original min(chunk_size, remaining.len())).
        let text = " hello world";
        let config = ChunkConfig {
            chunk_size: 5,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks[0], " hell");
        let reassembled: String = chunks.into_iter().collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn test_split_word_boundary_last_chunk_shorter() {
        // When the remaining text is shorter than chunk_size, word_boundary
        // logic is skipped (chunk_end == remaining.len(), so guard fails)
        let text = "aaaa bb";
        let config = ChunkConfig {
            chunk_size: 5,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks(text, &config);
        assert_eq!(chunks[0], "aaaa ");
        assert_eq!(chunks[1], "bb");
    }

    // ── empty input ──

    #[test]
    fn test_split_empty_string() {
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        let chunks = split_text_into_chunks("", &config);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_split_empty_string_word_boundary() {
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks("", &config);
        assert!(chunks.is_empty());
    }

    // ── ChunkConfig::default ──

    #[test]
    fn test_chunk_config_default() {
        let config = ChunkConfig::default();
        assert_eq!(config.chunk_size, 20);
        assert_eq!(config.chunk_delay_ms, 50);
        assert!(config.word_boundary);
    }

    // ── TextChunker::next_chunk (needs tokio runtime for Interval) ──

    #[tokio::test]
    async fn test_next_chunk_basic() {
        let config = ChunkConfig {
            chunk_size: 5,
            chunk_delay_ms: 1,
            word_boundary: false,
        };
        let mut chunker = TextChunker::new("Hello World!".to_string(), config);

        assert_eq!(chunker.next_chunk(), Some("Hello".to_string()));
        assert_eq!(chunker.next_chunk(), Some(" Worl".to_string()));
        assert_eq!(chunker.next_chunk(), Some("d!".to_string()));
        assert_eq!(chunker.next_chunk(), None);
    }

    #[tokio::test]
    async fn test_next_chunk_word_boundary() {
        let config = ChunkConfig {
            chunk_size: 8,
            chunk_delay_ms: 1,
            word_boundary: true,
        };
        let mut chunker = TextChunker::new("one two three four".to_string(), config);

        let mut collected = Vec::new();
        while let Some(chunk) = chunker.next_chunk() {
            collected.push(chunk);
        }
        let reassembled: String = collected.into_iter().collect();
        assert_eq!(reassembled, "one two three four");
    }

    #[tokio::test]
    async fn test_next_chunk_empty_text() {
        let config = ChunkConfig {
            chunk_size: 10,
            chunk_delay_ms: 1,
            word_boundary: false,
        };
        let mut chunker = TextChunker::new(String::new(), config);
        assert_eq!(chunker.next_chunk(), None);
    }

    #[tokio::test]
    async fn test_next_chunk_returns_none_after_exhausted() {
        let config = ChunkConfig {
            chunk_size: 100,
            chunk_delay_ms: 1,
            word_boundary: false,
        };
        let mut chunker = TextChunker::new("short".to_string(), config);
        assert_eq!(chunker.next_chunk(), Some("short".to_string()));
        assert_eq!(chunker.next_chunk(), None);
        assert_eq!(chunker.next_chunk(), None); // stays None
    }

    // ── Large text stress test ──

    // ── multi-byte UTF-8: both chunkers used to cut on byte indices ──

    /// Regression. `chunk_size` was compared against `remaining.len()` — a **byte**
    /// count — and the cut applied with `&remaining[..chunk_end]`, so any text whose
    /// cut landed inside a multi-byte sequence panicked:
    /// `end byte index 20 is not a char boundary; it is inside 'ス'`.
    /// `api::streaming_handler` chunks every assistant text block, so this aborted
    /// the streaming task for any non-ASCII answer long enough to be split.
    #[test]
    fn splitting_japanese_text_is_lossless() {
        let text = "日本語のテキストを分割する必要があります。".repeat(4);
        let chunks = split_text_into_chunks(&text, &ChunkConfig::default());
        assert!(chunks.len() > 1, "80 characters must be split");
        assert_eq!(chunks.concat(), text, "chunking must be lossless");
    }

    /// Whether the old code panicked depended on where the cut happened to fall, so
    /// sweep every alignment rather than trusting one lucky length.
    #[test]
    fn splitting_accented_french_is_lossless_at_every_chunk_size() {
        let text = "Déjà là, les élèves préfèrent répéter la leçon à côté du poêle.";
        for chunk_size in 1..=40 {
            for word_boundary in [false, true] {
                let config = ChunkConfig {
                    chunk_size,
                    chunk_delay_ms: 0,
                    word_boundary,
                };
                assert_eq!(
                    split_text_into_chunks(text, &config).concat(),
                    text,
                    "chunk_size={chunk_size}, word_boundary={word_boundary}"
                );
            }
        }
    }

    #[test]
    fn splitting_emoji_is_lossless() {
        let text = "🚀🇫🇷👨\u{200d}👩\u{200d}👧\u{200d}👦🎉";
        for chunk_size in 1..=12 {
            let config = ChunkConfig {
                chunk_size,
                chunk_delay_ms: 0,
                word_boundary: false,
            };
            let chunks = split_text_into_chunks(text, &config);
            assert_eq!(chunks.concat(), text, "chunk_size={chunk_size}");
            assert!(chunks.iter().all(|c| !c.is_empty()), "{chunks:?}");
        }
    }

    /// `ChunkConfig::chunk_size` is documented as a count of characters. It now is.
    #[test]
    fn chunk_size_counts_characters_not_bytes() {
        let text = "日本語テキスト";
        assert_eq!(text.len(), 21, "21 bytes");
        assert_eq!(text.chars().count(), 7, "7 characters");

        let config = ChunkConfig {
            chunk_size: 3,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        assert_eq!(
            split_text_into_chunks(text, &config),
            vec!["日本語", "テキス", "ト"]
        );
    }

    #[test]
    fn word_boundary_still_cuts_at_spaces_in_accented_text() {
        let config = ChunkConfig {
            chunk_size: 6,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        assert_eq!(
            split_text_into_chunks("été déjà fini", &config),
            vec!["été ", "déjà ", "fini"]
        );
    }

    // ── chunk_size: 0 ──

    /// Regression: a zero `chunk_size` produced a zero-length cut, so
    /// `split_text_into_chunks` advanced `position` by nothing and looped for ever
    /// pushing empty strings.
    #[test]
    fn a_zero_chunk_size_terminates_one_character_at_a_time() {
        for word_boundary in [false, true] {
            let config = ChunkConfig {
                chunk_size: 0,
                chunk_delay_ms: 0,
                word_boundary,
            };
            let chunks = split_text_into_chunks("abcdef", &config);
            assert_eq!(chunks, vec!["a", "b", "c", "d", "e", "f"]);
        }
    }

    // ── TextChunker::next_chunk — the same cut, through the stream ──

    #[tokio::test]
    async fn next_chunk_is_lossless_on_multibyte_text() {
        let config = ChunkConfig {
            chunk_size: 15,
            chunk_delay_ms: 1,
            word_boundary: true,
        };
        let text = "日本語のテキスト, et un peu de français accentué.".to_string();
        let mut chunker = TextChunker::new(text.clone(), config);

        let mut collected = String::new();
        while let Some(chunk) = chunker.next_chunk() {
            collected.push_str(&chunk);
        }
        assert_eq!(collected, text);
    }

    /// The two entry points used to carry two copies of the same arithmetic; they
    /// now share one `chunk_end`, so pin that they cannot drift apart.
    #[tokio::test]
    async fn the_two_chunkers_produce_the_same_pieces() {
        let text = "Un texte accentué, 日本語, et anticonstitutionnellement long.";
        for word_boundary in [false, true] {
            for chunk_size in [1usize, 2, 3, 7, 15, 20, 100] {
                let config = ChunkConfig {
                    chunk_size,
                    chunk_delay_ms: 1,
                    word_boundary,
                };
                let mut chunker = TextChunker::new(text.to_string(), config.clone());
                let mut streamed = Vec::new();
                while let Some(chunk) = chunker.next_chunk() {
                    streamed.push(chunk);
                }
                assert_eq!(
                    streamed,
                    split_text_into_chunks(text, &config),
                    "chunk_size={chunk_size}, word_boundary={word_boundary}"
                );
            }
        }
    }

    /// The forward-space branch: no space inside the chunk, so the cut moves right
    /// rather than splitting the word — a chunk may therefore exceed `chunk_size`.
    #[tokio::test]
    async fn next_chunk_looks_forward_when_the_chunk_holds_no_space() {
        let config = ChunkConfig {
            chunk_size: 4,
            chunk_delay_ms: 1,
            word_boundary: true,
        };
        let mut chunker = TextChunker::new("anticonstitutionnellement oui".to_string(), config);

        assert_eq!(
            chunker.next_chunk(),
            Some("anticonstitutionnellement ".to_string()),
            "a 25-character word is not cut at 4"
        );
        assert_eq!(chunker.next_chunk(), Some("oui".to_string()));
        assert_eq!(chunker.next_chunk(), None);
    }

    #[tokio::test]
    async fn a_zero_chunk_size_advances_one_character_at_a_time() {
        let config = ChunkConfig {
            chunk_size: 0,
            chunk_delay_ms: 1,
            word_boundary: false,
        };
        let mut chunker = TextChunker::new("héo".to_string(), config);

        assert_eq!(chunker.next_chunk(), Some("h".to_string()));
        assert_eq!(chunker.next_chunk(), Some("é".to_string()));
        assert_eq!(chunker.next_chunk(), Some("o".to_string()));
        assert_eq!(chunker.next_chunk(), None);
    }

    /// Regression: `tokio::time::interval` panics on a zero period, so building a
    /// chunker with the obvious "no delay" setting used to abort the task before a
    /// single chunk came out.
    #[tokio::test]
    async fn a_zero_delay_does_not_panic_on_construction() {
        let config = ChunkConfig {
            chunk_size: 2,
            chunk_delay_ms: 0,
            word_boundary: false,
        };
        let mut chunker = TextChunker::new("abcd".to_string(), config);
        assert_eq!(chunker.next_chunk(), Some("ab".to_string()));
    }

    // ── the Stream impl / chunk_text ──

    #[tokio::test(start_paused = true)]
    async fn chunk_text_streams_every_piece_in_order() {
        let config = ChunkConfig {
            chunk_size: 5,
            chunk_delay_ms: 30,
            word_boundary: false,
        };
        let pieces: Vec<String> = chunk_text("Café très chaud".to_string(), Some(config))
            .collect()
            .await;

        assert!(pieces.len() > 1, "the text must be split: {pieces:?}");
        assert_eq!(pieces.concat(), "Café très chaud");
    }

    /// `chunk_text(_, None)` must fall back to [`ChunkConfig::default`].
    #[tokio::test(start_paused = true)]
    async fn chunk_text_without_a_config_uses_the_default() {
        let text = "mot ".repeat(30);
        let pieces: Vec<String> = chunk_text(text.clone(), None).collect().await;

        assert_eq!(pieces.concat(), text);
        assert!(
            pieces.iter().all(|p| p.chars().count() <= 20),
            "default chunk_size is 20: {pieces:?}"
        );
    }

    /// The stream must end, not yield empty chunks for ever once the text is spent.
    #[tokio::test(start_paused = true)]
    async fn chunk_text_on_an_empty_string_yields_nothing() {
        let pieces: Vec<String> = chunk_text(String::new(), None).collect().await;
        assert!(pieces.is_empty());
    }

    #[test]
    fn test_split_large_text_reassembles() {
        let text = "word ".repeat(500);
        let text = text.trim_end(); // remove trailing space
        let config = ChunkConfig {
            chunk_size: 20,
            chunk_delay_ms: 0,
            word_boundary: true,
        };
        let chunks = split_text_into_chunks(text, &config);
        let reassembled: String = chunks.into_iter().collect();
        assert_eq!(reassembled, text);
    }
}
