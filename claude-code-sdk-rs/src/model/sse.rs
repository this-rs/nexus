//! Incremental Server-Sent Events decoder (bytes in, events out).
//!
//! Pure and synchronous: the HTTP layer feeds it whatever chunks the socket
//! delivers, in any size. A line may be cut anywhere (even inside a multi-byte
//! character or between `\r` and `\n`); the decoder buffers bytes and only
//! interprets complete lines.
//!
//! Supported: `\n` and `\r\n` line ends, `:` comment lines, `data:` (one optional
//! space after the colon is dropped), several `data:` lines per event (joined by
//! `\n`), `event:` and `id:` fields, and the `[DONE]` sentinel of OpenAI-style
//! streams (see [`SseEvent::is_done`]). A lone `\r` is not a line end here.

use crate::agent::ProviderError;

/// Largest amount of undelivered bytes the decoder accepts (one line or one event).
pub const MAX_BUFFERED_BYTES: usize = 16 * 1024 * 1024;

/// One decoded event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// Value of the `event:` field, when present.
    pub event: Option<String>,
    /// Value of the `id:` field, when present.
    pub id: Option<String>,
    /// The `data:` lines joined by `\n`.
    pub data: String,
}

impl SseEvent {
    /// Whether this is the `data: [DONE]` end-of-stream sentinel.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Incremental decoder. Create one per response.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    current: SseEvent,
    has_data: bool,
}

impl SseDecoder {
    /// A decoder with nothing buffered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes and returns the events completed by them.
    ///
    /// Fails (with a redacted [`ProviderError::Protocol`]) when more than
    /// [`MAX_BUFFERED_BYTES`] accumulate without a line end.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buffer[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            let mut line = &self.buffer[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = line.to_vec();
            start = end + 1;
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        self.buffer.drain(..start);
        if self.buffer.len() > MAX_BUFFERED_BYTES {
            return Err(ProviderError::protocol("SSE line exceeds the buffer limit"));
        }
        Ok(events)
    }

    /// Ends the stream: a final event not terminated by a blank line is delivered
    /// only when it carries data (the server closed right after its last line).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let mut rest = std::mem::take(&mut self.buffer);
            if rest.last() == Some(&b'\r') {
                rest.pop();
            }
            if let Some(event) = self.line(&rest) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let has_data = std::mem::take(&mut self.has_data);
        let event = std::mem::take(&mut self.current);
        has_data.then_some(event)
    }

    fn line(&mut self, line: &[u8]) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line[0] == b':' {
            return None;
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (text.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.current.data.push('\n');
                }
                self.current.data.push_str(value);
                self.has_data = true;
            },
            "event" => self.current.event = Some(value.to_string()),
            "id" => self.current.id = Some(value.to_string()),
            _ => {},
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk).unwrap());
        }
        events.extend(decoder.finish());
        events
    }

    fn data(events: &[SseEvent]) -> Vec<&str> {
        events.iter().map(|e| e.data.as_str()).collect()
    }

    #[test]
    fn decodes_two_events() {
        let events = decode_all(&[b"data: a\n\ndata: b\n\n"]);
        assert_eq!(data(&events), ["a", "b"]);
    }

    #[test]
    fn handles_crlf() {
        let events = decode_all(&[b"data: a\r\n\r\ndata: b\r\n\r\n"]);
        assert_eq!(data(&events), ["a", "b"]);
    }

    #[test]
    fn line_cut_between_two_chunks() {
        let events = decode_all(&[b"data: hel", b"lo\n", b"\ndata: x\n", b"\n"]);
        assert_eq!(data(&events), ["hello", "x"]);
    }

    #[test]
    fn crlf_cut_between_cr_and_lf() {
        let events = decode_all(&[b"data: a\r", b"\n\r", b"\n"]);
        assert_eq!(data(&events), ["a"]);
    }

    #[test]
    fn every_split_point_gives_the_same_events() {
        let stream =
            "data: h\u{e9}llo\r\n\r\n: ping\n\ndata: l1\ndata: l2\n\ndata: [DONE]\n\n".as_bytes();
        let whole = decode_all(&[stream]);
        for cut in 0..stream.len() {
            let split = decode_all(&[&stream[..cut], &stream[cut..]]);
            assert_eq!(split, whole, "cut at {cut}");
        }
        assert_eq!(data(&whole), ["h\u{e9}llo", "l1\nl2", "[DONE]"]);
    }

    #[test]
    fn comments_are_ignored_and_do_not_end_an_event() {
        let events = decode_all(&[b": keep-alive\n\n: x\ndata: a\n: y\n\n"]);
        assert_eq!(data(&events), ["a"]);
    }

    #[test]
    fn done_sentinel_is_recognised() {
        let events = decode_all(&[b"data: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert!(!events[0].is_done());
        assert!(events[1].is_done());
    }

    #[test]
    fn multiline_data_event_and_id_fields() {
        let events = decode_all(&[b"event: delta\nid: 7\ndata: a\ndata:b\n\n"]);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn unterminated_final_event_is_delivered_on_finish() {
        let events = decode_all(&[b"data: tail"]);
        assert_eq!(data(&events), ["tail"]);
    }

    #[test]
    fn empty_data_field_still_makes_an_event() {
        let events = decode_all(&[b"data:\n\n"]);
        assert_eq!(data(&events), [""]);
    }

    #[test]
    fn blank_lines_alone_make_no_event() {
        assert!(decode_all(&[b"\n\n\r\n"]).is_empty());
    }

    #[test]
    fn oversized_line_is_a_protocol_error() {
        let mut decoder = SseDecoder::new();
        let block = vec![b'a'; 1024 * 1024];
        let mut failure = None;
        for _ in 0..20 {
            if let Err(error) = decoder.push(&block) {
                failure = Some(error);
                break;
            }
        }
        assert!(matches!(failure, Some(ProviderError::Protocol { .. })));
    }
}
