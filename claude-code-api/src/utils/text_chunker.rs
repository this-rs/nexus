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
    pub fn new(text: String, config: ChunkConfig) -> Self {
        let interval = interval(Duration::from_millis(config.chunk_delay_ms));
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
        let mut chunk_end = self.config.chunk_size.min(remaining.len());

        // If word_boundary is enabled, try to break at word boundaries
        if self.config.word_boundary && chunk_end < remaining.len() {
            // Look for the last space within the chunk
            if let Some(last_space) = remaining[..chunk_end].rfind(' ') {
                if last_space > 0 {
                    chunk_end = last_space + 1; // Include the space
                }
            } else {
                // No space found in chunk, look forward for the next space
                if let Some(next_space) = remaining[chunk_end..].find(' ') {
                    chunk_end = chunk_end + next_space + 1;
                }
            }
        }

        let chunk = remaining[..chunk_end].to_string();
        self.position += chunk_end;
        Some(chunk)
    }
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
        let mut chunk_end = config.chunk_size.min(remaining.len());

        if config.word_boundary && chunk_end < remaining.len() {
            if let Some(last_space) = remaining[..chunk_end].rfind(' ') {
                if last_space > 0 {
                    chunk_end = last_space + 1;
                }
            } else if let Some(next_space) = remaining[chunk_end..].find(' ') {
                chunk_end = chunk_end + next_space + 1;
            }
        }

        chunks.push(remaining[..chunk_end].to_string());
        position += chunk_end;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

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
