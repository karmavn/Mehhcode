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
    scan_offset: usize,
    current_event: Option<String>,
    current_data: String,
    has_data: bool,
    max_buffer_bytes: usize,
    overflowed: bool,
    incomplete_at_eof: bool,
    at_stream_start: bool,
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
            raw_buffer: Vec::with_capacity(4096),
            scan_offset: 0,
            current_event: None,
            current_data: String::new(),
            has_data: false,
            max_buffer_bytes: MAX_SSE_BUFFER_BYTES,
            overflowed: false,
            incomplete_at_eof: false,
            at_stream_start: true,
        }
    }

    /// Creates a new SSE parser with a custom buffer limit in bytes.
    pub fn with_max_buffer_bytes(max_buffer_bytes: usize) -> Self {
        Self {
            max_buffer_bytes,
            ..Self::new()
        }
    }

    fn buffered_bytes(&self) -> usize {
        self.current_data
            .len()
            .saturating_add(self.current_event.as_ref().map_or(0, String::len))
    }

    fn reset_on_overflow(&mut self) {
        self.overflowed = true;
        self.raw_buffer.clear();
        self.scan_offset = 0;
        self.current_event = None;
        self.current_data.clear();
        self.has_data = false;
    }

    /// Returns true if buffered SSE data exceeded `max_buffer_bytes`.
    pub fn has_overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns true if any incomplete line, truncated field, or unflushed event data remains at EOF.
    pub fn has_pending_bytes(&self) -> bool {
        self.incomplete_at_eof
            || !self.raw_buffer.is_empty()
            || self.has_data
            || self.current_event.is_some()
    }

    /// Feeds raw bytes into the parser, decoding UTF-8 only on complete `\n`-terminated lines.
    pub fn feed_bytes(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        if self.overflowed {
            return Vec::new();
        }
        if self
            .raw_buffer
            .len()
            .saturating_add(chunk.len())
            .saturating_add(self.buffered_bytes())
            > self.max_buffer_bytes
        {
            self.reset_on_overflow();
            return Vec::new();
        }

        self.raw_buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut raw_buf = std::mem::take(&mut self.raw_buffer);
        let mut cursor = 0usize;
        let mut search_from = self.scan_offset.min(raw_buf.len());

        while let Some(rel_pos) = raw_buf[search_from..].iter().position(|&b| b == b'\n') {
            let pos = search_from + rel_pos;
            let mut line_end = pos;
            if line_end > cursor && raw_buf[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let next_cursor = pos + 1;
            let remaining_raw_bytes = raw_buf.len() - next_cursor;
            let line = String::from_utf8_lossy(&raw_buf[cursor..line_end]);
            let line = self.strip_initial_bom(line.as_ref());
            self.process_line(line, remaining_raw_bytes, &mut events);
            cursor = next_cursor;
            search_from = next_cursor;
            if self.overflowed {
                // Stop parsing further lines in this chunk; callers check
                // `has_overflowed()` and fail closed immediately.
                return events;
            }
        }

        if cursor > 0 {
            raw_buf.drain(..cursor);
        }
        self.scan_offset = raw_buf.len();
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
        self.scan_offset = 0;
        if !self.raw_buffer.is_empty() {
            let mut line_bytes = std::mem::take(&mut self.raw_buffer);
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let line = String::from_utf8(line_bytes)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            let line = self.strip_initial_bom(&line);
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
            self.process_line(line, 0, &mut events);
        }
        if self.has_data {
            events.push(SseEvent {
                event: self.current_event.take(),
                data: std::mem::take(&mut self.current_data),
            });
            self.has_data = false;
        } else if self.current_event.is_some() {
            self.current_event = None;
            self.incomplete_at_eof = true;
        }
        events
    }

    /// Removes a UTF-8 BOM from the first line of the stream, if present.
    /// BOMs appearing later in the stream are left untouched.
    fn strip_initial_bom<'a>(&mut self, line: &'a str) -> &'a str {
        if self.at_stream_start {
            self.at_stream_start = false;
            line.strip_prefix('\u{FEFF}').unwrap_or(line)
        } else {
            line
        }
    }

    fn append_data_line(&mut self, val: &str, remaining_raw_bytes: usize) {
        let extra = val.len().saturating_add(usize::from(self.has_data));
        let next_total = remaining_raw_bytes
            .saturating_add(self.buffered_bytes())
            .saturating_add(extra.max(1));
        if next_total > self.max_buffer_bytes {
            self.reset_on_overflow();
            return;
        }
        if self.has_data {
            self.current_data.push('\n');
        }
        self.current_data.push_str(val);
        self.has_data = true;
    }

    fn process_line(
        &mut self,
        line: &str,
        remaining_raw_bytes: usize,
        events: &mut Vec<SseEvent>,
    ) {
        if line.is_empty() {
            // An empty line signals the boundary between SSE events
            if self.has_data {
                events.push(SseEvent {
                    event: self.current_event.take(),
                    data: std::mem::take(&mut self.current_data),
                });
                self.has_data = false;
            } else {
                self.current_event = None;
            }
            return;
        }

        if line.starts_with(':') {
            // Lines starting with colon are SSE comments/heartbeats and must be ignored
            return;
        }

        if let Some(rest) = line.strip_prefix("data:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            self.append_data_line(val, remaining_raw_bytes);
        } else if line == "data" {
            self.append_data_line("", remaining_raw_bytes);
        } else if let Some(rest) = line.strip_prefix("event:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
            let next_total = remaining_raw_bytes
                .saturating_add(self.current_data.len())
                .saturating_add(val.len());
            if next_total > self.max_buffer_bytes {
                self.reset_on_overflow();
                return;
            }
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
        let payload = "data: hello \u{4E16}\u{754C} \u{1F389}\n\n".as_bytes();
        // Split right inside "\u{4E16}" (bytes 12..15) and inside "\u{1F389}"
        let split1 = 13;
        let split2 = payload.len() - 4;
        assert!(parser.feed_bytes(&payload[..split1]).is_empty());
        assert!(parser.feed_bytes(&payload[split1..split2]).is_empty());
        let events = parser.feed_bytes(&payload[split2..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello \u{4E16}\u{754C} \u{1F389}");
        assert!(!events[0].data.contains('\u{FFFD}'));
    }

    #[test]
    fn test_bare_data_and_event_lines_respect_buffer_cap() {
        let mut parser = SseParser::with_max_buffer_bytes(16);
        let chunk = "data\n".repeat(20);
        let _ = parser.feed(&chunk);
        assert!(parser.has_overflowed());

        let mut ev_parser = SseParser::with_max_buffer_bytes(16);
        let _ = ev_parser.feed("event: 12345678901234567\n");
        assert!(ev_parser.has_overflowed());
    }

    #[test]
    fn test_leading_bom_does_not_drop_first_event() {
        let mut parser = SseParser::new();
        let mut stream = vec![0xEF, 0xBB, 0xBF];
        stream.extend_from_slice(b"data: {\"type\":\"message_start\"}\n\n");
        let events = parser.feed_bytes(&stream);
        assert_eq!(
            events.len(),
            1,
            "the first event after a leading BOM must not be dropped"
        );
        assert_eq!(events[0].data, "{\"type\":\"message_start\"}");
        assert!(!events[0].data.contains('\u{FFFD}'));
    }

    #[test]
    fn test_feed_bytes_caps_buffer() {
        let mut parser = SseParser::with_max_buffer_bytes(64);
        let events = parser.feed_bytes(&[b'a'; 100]);
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

    #[test]
    fn empty_data_lines_count_towards_buffer_limit() {
        const LINE_COUNT: usize = 200;
        const MAX: usize = 64;

        let mut with_colon = SseParser::with_max_buffer_bytes(MAX);
        for _ in 0..LINE_COUNT {
            with_colon.feed("data:\n");
        }
        assert!(
            with_colon.has_overflowed(),
            "repeated `data:` lines must trip the buffer guard"
        );

        let mut with_space = SseParser::with_max_buffer_bytes(MAX);
        for _ in 0..LINE_COUNT {
            with_space.feed("data: \n");
        }
        assert!(
            with_space.has_overflowed(),
            "repeated `data: ` lines must trip the buffer guard"
        );

        let mut bare = SseParser::with_max_buffer_bytes(MAX);
        for _ in 0..LINE_COUNT {
            bare.feed("data\n");
        }
        assert!(
            bare.has_overflowed(),
            "repeated bare `data` lines must trip the buffer guard"
        );
    }

    #[test]
    fn large_single_line_chunked_sse_parses_cleanly() {
        let mut parser = SseParser::new();
        let payload = "x".repeat(256 * 1024);
        let frame = format!("data: {payload}\n\n");
        let mut events = Vec::new();
        for chunk in frame.as_bytes().chunks(128) {
            events.extend(parser.feed_bytes(chunk));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, payload);
        assert!(!parser.has_pending_bytes());
    }
}
