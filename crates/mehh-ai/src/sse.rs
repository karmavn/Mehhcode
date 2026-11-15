//! Server-Sent Events (SSE) streaming parser.

/// Default maximum buffered bytes for incomplete SSE lines and multi-line events (16 MiB).
pub const MAX_SSE_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// A parsed Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional event type name specified by `event:` lines.
    pub event: Option<String>,
    /// The event payload accumulated from one or more `data:` lines.
    pub data: String,
}

/// Incremental byte-level parser for Server-Sent Events streams.
#[derive(Debug)]
pub struct SseParser {
    raw_buffer: Vec<u8>,
    current_event: Option<String>,
    current_data: Vec<String>,
    data_bytes: usize,
    max_buffer_bytes: usize,
    overflowed: bool,
    incomplete_at_eof: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    /// Creates a new empty SSE parser with the default 16 MiB buffer limit.
    pub fn new() -> Self {
        Self {
            raw_buffer: Vec::new(),
            current_event: None,
            current_data: Vec::new(),
            data_bytes: 0,
            max_buffer_bytes: MAX_SSE_BUFFER_BYTES,
            overflowed: false,
            incomplete_at_eof: false,
        }
    }

    /// Creates a new SSE parser with a custom buffer limit in bytes.
    pub fn with_max_buffer_bytes(max_buffer_bytes: usize) -> Self {
        Self {
            max_buffer_bytes,
            ..Self::new()
        }
    }

    /// Returns true if buffered SSE data exceeded `max_buffer_bytes`.
    pub fn has_overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns true if any incomplete line, truncated field, or unflushed event data remains at EOF.
    pub fn has_pending_bytes(&self) -> bool {
        self.incomplete_at_eof
            || !self.raw_buffer.is_empty()
            || !self.current_data.is_empty()
            || self.current_event.is_some()
    }

    /// Feeds raw bytes into the parser, decoding UTF-8 only on complete `\n`-terminated lines.
    pub fn feed_bytes(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        if self.overflowed {
            return Vec::new();
        }
        if self.raw_buffer.len().saturating_add(chunk.len()).saturating_add(self.data_bytes)
            > self.max_buffer_bytes
        {
            self.overflowed = true;
            self.raw_buffer.clear();
            self.current_event = None;
            self.current_data.clear();
            self.data_bytes = 0;
            return Vec::new();
        }

        self.raw_buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut raw_buf = std::mem::take(&mut self.raw_buffer);
        let mut cursor = 0usize;

        while let Some(rel_pos) = raw_buf[cursor..].iter().position(|&b| b == b'\n') {
            let pos = cursor + rel_pos;
            let mut line_end = pos;
            if line_end > cursor && raw_buf[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let next_cursor = pos + 1;
            let remaining_raw_bytes = raw_buf.len() - next_cursor;
            let line = String::from_utf8_lossy(&raw_buf[cursor..line_end]);
            self.process_line(&line, remaining_raw_bytes, &mut events);
            cursor = next_cursor;
            if self.overflowed {
                return Vec::new();
            }
        }

        if cursor > 0 {
            raw_buf.drain(..cursor);
        }
        self.raw_buffer = raw_buf;

        events
    }

    /// Feeds a chunk of text into the parser and returns any completed SSE events.
    pub fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.feed_bytes(chunk.as_bytes())
    }

    /// Flushes any remaining buffered data and returns any final event.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        if self.overflowed {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.raw_buffer.is_empty() {
            let mut line_bytes = std::mem::take(&mut self.raw_buffer);
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8(line_bytes)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            let recognized = line.is_empty()
                || line.starts_with(':')
                || line == "data"
                || line.starts_with("data:")
                || line == "event"
                || line.starts_with("event:")
                || line == "id"
                || line.starts_with("id:")
                || line == "retry"
                || line.starts_with("retry:");
            if !recognized {
                self.incomplete_at_eof = true;
            }
            self.process_line(&line, 0, &mut events);
        }
        if !self.current_data.is_empty() {
            events.push(SseEvent {
                event: self.current_event.take(),
                data: self.current_data.join("\n"),
            });
            self.current_data.clear();
            self.data_bytes = 0;
        } else if self.current_event.is_some() {
            self.current_event = None;
            self.incomplete_at_eof = true;
        }
        events
    }

    fn process_line(
        &mut self,
        line: &str,
        remaining_raw_bytes: usize,
        events: &mut Vec<SseEvent>,
    ) {
        if line.is_empty() {
            // An empty line signals the boundary between SSE events
            if !self.current_data.is_empty() || self.current_event.is_some() {
                events.push(SseEvent {
                    event: self.current_event.take(),
                    data: self.current_data.join("\n"),
                });
                self.current_data.clear();
                self.data_bytes = 0;
            }
            return;
        }

        if line.starts_with(':') {
            // Lines starting with colon are SSE comments/heartbeats and must be ignored
            return;
        }

        if let Some(rest) = line.strip_prefix("data:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            self.data_bytes = self.data_bytes.saturating_add(val.len());
            if remaining_raw_bytes.saturating_add(self.data_bytes) > self.max_buffer_bytes {
                self.overflowed = true;
                self.raw_buffer.clear();
                self.current_event = None;
                self.current_data.clear();
                self.data_bytes = 0;
                return;
            }
            self.current_data.push(val.to_string());
        } else if line == "data" {
            self.current_data.push(String::new());
        } else if let Some(rest) = line.strip_prefix("event:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            self.current_event = Some(val.to_string());
        } else if line == "event" {
            self.current_event = Some(String::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: hello world\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "hello world");
    }

    #[test]
    fn test_event_with_type_and_crlf() {
        let mut parser = SseParser::new();
        let events = parser.feed("event: message_start\r\ndata: {\"type\": \"test\"}\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"type\": \"test\"}");
    }

    #[test]
    fn test_multiline_data() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: line 1\ndata: line 2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "line 1\nline 2");
    }

    #[test]
    fn test_chunked_splits_across_lines() {
        let mut parser = SseParser::new();
        assert!(parser.feed("data: hel").is_empty());
        assert!(parser.feed("lo").is_empty());
        assert!(parser.feed(" world\n").is_empty());
        let events = parser.feed("\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello world");
    }

    #[test]
    fn test_comments_and_blank_lines() {
        let mut parser = SseParser::new();
        let chunk = ": ping heartbeat\n\n: another comment\nevent: ping\ndata: pong\n\n";
        let events = parser.feed(chunk);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "pong");
    }

    #[test]
    fn test_finish_flushes_partial_event() {
        let mut parser = SseParser::new();
        assert!(parser.feed("event: end\ndata: final chunk").is_empty());
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("end"));
        assert_eq!(events[0].data, "final chunk");
    }

    #[test]
    fn test_feed_bytes_preserves_split_multibyte_utf8() {
        let mut parser = SseParser::new();
        let payload = "data: hello 世界 🎉\n\n".as_bytes();
        // Split right inside "世" (bytes 12..15) and inside "🎉"
        let split1 = 13;
        let split2 = payload.len() - 4;
        assert!(parser.feed_bytes(&payload[..split1]).is_empty());
        assert!(parser.feed_bytes(&payload[split1..split2]).is_empty());
        let events = parser.feed_bytes(&payload[split2..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello 世界 🎉");
        assert!(!events[0].data.contains('\u{FFFD}'));
    }

    #[test]
    fn test_feed_bytes_caps_buffer() {
        let mut parser = SseParser::with_max_buffer_bytes(64);
        let events = parser.feed_bytes(&vec![b'a'; 100]);
        assert!(events.is_empty());
        assert!(parser.has_overflowed());
    }

    #[test]
    fn test_finish_marks_incomplete_at_eof_for_truncated_frame() {
        let mut parser = SseParser::new();
        assert!(parser.feed_bytes(b"event: content_block_delta\nda").is_empty());
        let events = parser.finish();
        assert!(events.is_empty());
        assert!(parser.has_pending_bytes());
    }
}
