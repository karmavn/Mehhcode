//! Server-Sent Events (SSE) streaming parser.

/// A parsed Server-Sent Event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional event type name specified by `event:` lines.
    pub event: Option<String>,
    /// The event payload accumulated from one or more `data:` lines.
    pub data: String,
}

/// Incremental parser for Server-Sent Events streams.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
    current_event: Option<String>,
    current_data: Vec<String>,
}

impl SseParser {
    /// Creates a new empty SSE parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk of text into the parser and returns any completed SSE events.
    pub fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].to_string();
            self.buffer.drain(..=pos);
            let trimmed = line.strip_suffix('\r').unwrap_or(&line);
            self.process_line(trimmed, &mut events);
        }

        events
    }

    /// Flushes any remaining buffered data and returns any final event.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let trimmed = line.strip_suffix('\r').unwrap_or(&line);
            self.process_line(trimmed, &mut events);
        }
        if !self.current_data.is_empty() || self.current_event.is_some() {
            events.push(SseEvent {
                event: self.current_event.take(),
                data: self.current_data.join("\n"),
            });
            self.current_data.clear();
        }
        events
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            // An empty line signals the boundary between SSE events
            if !self.current_data.is_empty() || self.current_event.is_some() {
                events.push(SseEvent {
                    event: self.current_event.take(),
                    data: self.current_data.join("\n"),
                });
                self.current_data.clear();
            }
            return;
        }

        if line.starts_with(':') {
            // Lines starting with colon are SSE comments/heartbeats and must be ignored
            return;
        }

        if let Some(rest) = line.strip_prefix("data:") {
            let val = rest.strip_prefix(' ').unwrap_or(rest);
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
}
