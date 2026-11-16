//! Streaming incremental JSON parser, JSON repair helper, and tool call arguments streaming.

use serde_json::{Map, Value};

/// Repairs malformed JSON string literals by:
/// - escaping raw control characters inside strings
/// - doubling backslashes before invalid escape characters
pub fn repair_json(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut chars = json.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if !in_string {
            repaired.push(ch);
            if ch == '"' {
                in_string = true;
            }
            continue;
        }

        if ch == '"' {
            repaired.push(ch);
            in_string = false;
            continue;
        }

        if ch == '\\' {
            match chars.peek().map(|&(_, c)| c) {
                None => {
                    repaired.push_str("\\\\");
                    continue;
                }
                Some('u') => {
                    let next_byte_idx = chars.peek().unwrap().0;
                    let remaining = &json[next_byte_idx..];
                    if remaining.len() >= 5
                        && remaining.as_bytes()[1..5].iter().all(|b| b.is_ascii_hexdigit())
                    {
                        repaired.push('\\');
                        repaired.push_str(&remaining[..5]);
                        for _ in 0..5 {
                            chars.next();
                        }
                        continue;
                    }
                    repaired.push_str("\\\\");
                    continue;
                }
                Some(next_ch) if matches!(next_ch, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => {
                    repaired.push('\\');
                    repaired.push(next_ch);
                    chars.next();
                    continue;
                }
                Some(_) => {
                    repaired.push_str("\\\\");
                    continue;
                }
            }
        }

        if (ch as u32) <= 0x1f {
            match ch {
                '\x08' => repaired.push_str("\\b"),
                '\x0c' => repaired.push_str("\\f"),
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                _ => repaired.push_str(&format!("\\u{:04x}", ch as u32)),
            }
        } else {
            repaired.push(ch);
        }
    }

    repaired
}

/// Parses the model's complete arguments text of a finished tool call.
/// Returns (arguments, arguments_error).
pub fn parse_tool_call_arguments(
    json: Option<&str>,
) -> (Map<String, Value>, Option<String>) {
    let Some(text) = json else {
        return (Map::new(), None);
    };
    if text.trim().is_empty() {
        return (Map::new(), None);
    }

    let parsed_val: Result<Value, _> = serde_json::from_str(text).or_else(|orig_err| {
        let repaired = repair_json(text);
        if repaired != text {
            serde_json::from_str(&repaired).map_err(|_| orig_err)
        } else {
            Err(orig_err)
        }
    });

    match parsed_val {
        Ok(Value::Object(map)) => (map, None),
        Ok(_) => (
            Map::new(),
            Some(format!(
                "Tool arguments must be a JSON object\n\nReceived arguments:\n{}",
                text
            )),
        ),
        Err(err) => (
            Map::new(),
            Some(format!("{}\n\nReceived arguments:\n{}", err, text)),
        ),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Slot {
    Root,
    Member(String),
    Element(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Key,
    Colon,
    Value,
    Comma,
}

#[derive(Debug, Clone)]
enum Container {
    Object(Map<String, Value>),
    Array(Vec<Value>),
}

#[derive(Debug, Clone)]
struct Frame {
    container: Container,
    slot: Option<Slot>,
    size: usize,
    key: Option<String>,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringTarget {
    Key,
    Value,
}

#[derive(Debug, Clone)]
struct OpenString {
    target: StringTarget,
    slot: Option<Slot>,
    text: String,
    escape: String,
    pending_surrogate: Option<u16>,
}

const CREDIT_PER_APPEND: usize = 256;
const CREDIT_PER_CHARACTER: usize = 8;
const MAX_PENDING_SCALAR_LENGTH: usize = 64;

fn simple_escape(ch: char) -> Option<&'static str> {
    match ch {
        '"' => Some("\""),
        '\\' => Some("\\"),
        '/' => Some("/"),
        'b' => Some("\x08"),
        'f' => Some("\x0c"),
        'n' => Some("\n"),
        'r' => Some("\r"),
        't' => Some("\t"),
        _ => None,
    }
}

fn is_delimiter(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '\r' | ',' | ':' | '{' | '}' | '[' | ']' | '"'
    )
}

fn scalar_value(token: &str) -> Option<Value> {
    match token {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        "null" => Some(Value::Null),
        _ => {
            if let Ok(val) = serde_json::from_str::<Value>(token) {
                if val.is_number() {
                    return Some(val);
                }
            }
            None
        }
    }
}

fn pending_scalar_value(token: &str) -> Option<Value> {
    if !token.is_empty() {
        if "true".starts_with(token) {
            return Some(Value::Bool(true));
        }
        if "false".starts_with(token) {
            return Some(Value::Bool(false));
        }
        if "null".starts_with(token) {
            return Some(Value::Null);
        }
    }

    if let Ok(val) = serde_json::from_str::<Value>(token) {
        if val.is_number() {
            return Some(val);
        }
    }

    if let Some(pos) = token.rfind(['e', 'E']) {
        let suffix = &token[pos + 1..];
        if suffix.is_empty() || suffix == "+" || suffix == "-" {
            let mantissa = &token[..pos];
            if let Ok(val) = serde_json::from_str::<Value>(mantissa) {
                if val.is_number() {
                    return Some(val);
                }
            }
        }
    }

    None
}

fn set_slot(container: &mut Container, slot: &Slot, value: Value) {
    match (container, slot) {
        (Container::Array(elements), Slot::Element(index)) => {
            if *index == elements.len() {
                elements.push(value);
            } else if *index < elements.len() {
                elements[*index] = value;
            }
        }
        (Container::Object(members), Slot::Member(key)) => {
            members.insert(key.clone(), value);
        }
        _ => {}
    }
}

/// Incremental, tolerant streaming JSON parser.
#[derive(Debug, Default)]
pub struct StreamingJsonParser {
    root: Option<Value>,
    done: bool,
    stack: Vec<Frame>,
    open_string: Option<OpenString>,
    scalar: String,
    scalar_slot: Option<Slot>,
    path_cost: usize,
    credit: usize,
    dirty: bool,
    snapshot_cache: Option<Value>,
}

impl StreamingJsonParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, text: &str) {
        self.credit += CREDIT_PER_APPEND + CREDIT_PER_CHARACTER * text.len();
        let mut index = 0;
        while index < text.len() && !self.done {
            if self.open_string.is_some() {
                index = self.read_string(text, index);
            } else if !self.scalar.is_empty() {
                index = self.read_scalar(text, index);
            } else {
                let ch = text[index..].chars().next().unwrap();
                index += ch.len_utf8();
                self.structural(ch);
            }
        }
    }

    pub fn snapshot(&mut self) -> Option<Value> {
        if !self.dirty {
            return self.snapshot_cache.clone();
        }
        let pending = self.pending_scalar();
        let cost = self.path_cost + if pending.is_some() { 1 } else { 0 };
        if cost > self.credit {
            return self.snapshot_cache.clone();
        }
        self.credit -= cost;
        self.dirty = false;
        self.snapshot_cache = self.copy_open_path(pending);
        self.snapshot_cache.clone()
    }

    fn read_string(&mut self, text: &str, start: usize) -> usize {
        let mut run_start = start;

        for (offset, ch) in text[start..].char_indices() {
            let abs_index = start + offset;

            if !self.open_string.as_ref().unwrap().escape.is_empty() {
                if self.continue_escape(ch) {
                    run_start = abs_index + ch.len_utf8();
                    continue;
                }
                run_start = abs_index;
            }

            if ch == '"' {
                if abs_index > run_start {
                    self.append_string(&text[run_start..abs_index]);
                }
                self.end_string();
                return abs_index + 1;
            }

            if ch == '\\' {
                if abs_index > run_start {
                    self.append_string(&text[run_start..abs_index]);
                }
                self.open_string.as_mut().unwrap().escape.push('\\');
                run_start = abs_index + 1;
            }
        }

        if text.len() > run_start {
            self.append_string(&text[run_start..]);
        }

        text.len()
    }

    fn continue_escape(&mut self, ch: char) -> bool {
        let escape_state = self.open_string.as_ref().unwrap().escape.clone();
        if escape_state == "\\" {
            if let Some(decoded) = simple_escape(ch) {
                self.open_string.as_mut().unwrap().escape.clear();
                self.append_string(decoded);
                return true;
            }
            if ch == 'u' {
                self.open_string.as_mut().unwrap().escape.push('u');
                return true;
            }
            self.open_string.as_mut().unwrap().escape.clear();
            self.append_string("\\");
            return false;
        }

        if ch.is_ascii_hexdigit() {
            let open = self.open_string.as_mut().unwrap();
            open.escape.push(ch);
            if open.escape.len() < 6 {
                return true;
            }
            let hex_str = open.escape[2..6].to_string();
            open.escape.clear();

            if let Ok(code) = u32::from_str_radix(&hex_str, 16) {
                if let Some(c) = char::from_u32(code) {
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf).to_string();
                    self.append_string(&s);
                } else if (0xd800..=0xdbff).contains(&code) {
                    self.open_string.as_mut().unwrap().pending_surrogate = Some(code as u16);
                } else if (0xdc00..=0xdfff).contains(&code) {
                    let high = self.open_string.as_mut().unwrap().pending_surrogate.take();
                    if let Some(h) = high {
                        let combined = 0x10000 + (((h as u32) - 0xd800) << 10) + (code - 0xdc00);
                        if let Some(c) = char::from_u32(combined) {
                            let mut buf = [0u8; 4];
                            let s = c.encode_utf8(&mut buf).to_string();
                            self.append_string(&s);
                        }
                    }
                }
            }
            return true;
        }

        let literal = std::mem::take(&mut self.open_string.as_mut().unwrap().escape);
        self.append_string(&literal);
        false
    }

    fn append_string(&mut self, text: &str) {
        let (target, slot, open_text) = {
            let open = self.open_string.as_mut().unwrap();
            open.text.push_str(text);
            (open.target, open.slot.clone(), open.text.clone())
        };
        if target == StringTarget::Value {
            self.place(slot, Value::String(open_text));
        }
    }

    fn end_string(&mut self) {
        let open = self.open_string.take().unwrap();
        if open.target == StringTarget::Key {
            if let Some(frame) = self.stack.last_mut() {
                frame.key = Some(open.text);
                frame.phase = Phase::Colon;
            }
            return;
        }
        self.value_ended();
    }

    fn read_scalar(&mut self, text: &str, start: usize) -> usize {
        let mut end = start;
        for (offset, ch) in text[start..].char_indices() {
            if is_delimiter(ch) {
                end = start + offset;
                break;
            }
            end = start + offset + ch.len_utf8();
        }
        if end > start {
            self.scalar.push_str(&text[start..end]);
            if self.scalar_slot.is_some() {
                self.dirty = true;
            }
        }
        if end < text.len() {
            self.end_scalar();
        }
        end
    }

    fn end_scalar(&mut self) {
        let token = std::mem::take(&mut self.scalar);
        let slot = self.scalar_slot.take();
        if let Some(val) = scalar_value(&token) {
            self.place(slot, val);
        }
        self.value_ended();
    }

    fn pending_scalar(&self) -> Option<(Slot, Value)> {
        let slot = self.scalar_slot.as_ref()?;
        if self.scalar.is_empty() || self.scalar.len() > MAX_PENDING_SCALAR_LENGTH {
            return None;
        }
        let val = pending_scalar_value(&self.scalar)?;
        Some((slot.clone(), val))
    }

    fn structural(&mut self, ch: char) {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {}
            '"' => self.start_string(),
            '{' => self.open_container(Container::Object(Map::new())),
            '[' => self.open_container(Container::Array(Vec::new())),
            '}' | ']' => self.close_container(),
            ':' => {
                if let Some(frame) = self.stack.last_mut() {
                    if matches!(frame.container, Container::Object(_)) && frame.phase == Phase::Colon {
                        frame.phase = Phase::Value;
                    }
                }
            }
            ',' => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.phase = if matches!(frame.container, Container::Array(_)) {
                        Phase::Value
                    } else {
                        Phase::Key
                    };
                }
            }
            _ => {
                self.scalar.push(ch);
                self.scalar_slot = self.value_slot();
                if self.scalar_slot.is_some() {
                    self.dirty = true;
                }
            }
        }
    }

    fn value_slot(&self) -> Option<Slot> {
        let Some(frame) = self.stack.last() else {
            return if self.root.is_none() {
                Some(Slot::Root)
            } else {
                None
            };
        };

        match &frame.container {
            Container::Array(elements) => Some(Slot::Element(elements.len())),
            Container::Object(_) => {
                if (frame.phase == Phase::Value || frame.phase == Phase::Colon) && frame.key.is_some() {
                    Some(Slot::Member(frame.key.clone().unwrap()))
                } else {
                    None
                }
            }
        }
    }

    fn start_string(&mut self) {
        if let Some(frame) = self.stack.last() {
            if matches!(frame.container, Container::Object(_))
                && (frame.phase == Phase::Key || frame.phase == Phase::Comma)
            {
                self.open_string = Some(OpenString {
                    target: StringTarget::Key,
                    slot: None,
                    text: String::new(),
                    escape: String::new(),
                    pending_surrogate: None,
                });
                return;
            }
        }

        let slot = self.value_slot();
        self.open_string = Some(OpenString {
            target: StringTarget::Value,
            slot: slot.clone(),
            text: String::new(),
            escape: String::new(),
            pending_surrogate: None,
        });
        self.place(slot, Value::String(String::new()));
    }

    fn open_container(&mut self, container: Container) {
        let slot = self.value_slot();
        let is_array = matches!(container, Container::Array(_));
        self.stack.push(Frame {
            container,
            slot,
            size: 0,
            key: None,
            phase: if is_array { Phase::Value } else { Phase::Key },
        });
        self.path_cost += 1;
        self.dirty = true;
    }

    fn close_container(&mut self) {
        let Some(frame) = self.stack.pop() else { return; };
        self.path_cost = self.path_cost.saturating_sub(frame.size + 1);
        self.dirty = true;
        let val = match frame.container {
            Container::Object(m) => Value::Object(m),
            Container::Array(a) => Value::Array(a),
        };
        self.place(frame.slot, val);
        self.value_ended();
    }

    fn value_ended(&mut self) {
        if self.stack.is_empty() {
            if self.root.is_some() {
                self.done = true;
            }
            return;
        }
        self.stack.last_mut().unwrap().phase = Phase::Comma;
    }

    fn place(&mut self, slot: Option<Slot>, value: Value) {
        let Some(slot) = slot else { return; };
        self.dirty = true;
        match slot {
            Slot::Root => {
                self.root = Some(value);
            }
            Slot::Element(index) => {
                if let Some(frame) = self.stack.last_mut() {
                    if let Container::Array(elements) = &mut frame.container {
                        if index == elements.len() {
                            frame.size += 1;
                            self.path_cost += 1;
                            elements.push(value);
                        } else if index < elements.len() {
                            elements[index] = value;
                        }
                    }
                }
            }
            Slot::Member(key) => {
                if let Some(frame) = self.stack.last_mut() {
                    if let Container::Object(members) = &mut frame.container {
                        if !members.contains_key(&key) {
                            frame.size += 1;
                            self.path_cost += 1;
                        }
                        members.insert(key, value);
                    }
                }
            }
        }
    }

    fn copy_open_path(&self, pending: Option<(Slot, Value)>) -> Option<Value> {
        if self.stack.is_empty() {
            return match pending {
                Some((Slot::Root, val)) => Some(val),
                _ => self.root.clone(),
            };
        }

        let mut child: Option<Value> = None;
        let mut child_slot: Option<Slot> = None;

        for depth in (0..self.stack.len()).rev() {
            let frame = &self.stack[depth];
            let mut copy = frame.container.clone();

            if depth == self.stack.len() - 1 {
                if let Some((ref slot, ref val)) = pending {
                    set_slot(&mut copy, slot, val.clone());
                }
            }

            if let (Some(c), Some(ref cs)) = (child, child_slot) {
                set_slot(&mut copy, cs, c);
            }

            child = Some(match copy {
                Container::Object(m) => Value::Object(m),
                Container::Array(a) => Value::Array(a),
            });
            child_slot = frame.slot.clone();
        }

        match child_slot {
            Some(Slot::Root) => child,
            _ => self.root.clone(),
        }
    }
}

/// Helper tracking streaming tool call argument chunks.
#[derive(Debug, Default)]
pub struct ToolCallArgumentsStream {
    text: String,
    parser: StreamingJsonParser,
    snapshot: Option<Value>,
    arguments: Map<String, Value>,
}

impl ToolCallArgumentsStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn append(&mut self, delta: &str) -> Map<String, Value> {
        self.text.push_str(delta);
        self.parser.append(delta);
        self.update_partial()
    }

    pub fn replace(&mut self, text: &str) -> Map<String, Value> {
        if text.starts_with(&self.text) {
            let delta = &text[self.text.len()..];
            if delta.is_empty() {
                self.partial()
            } else {
                self.append(delta)
            }
        } else {
            self.text.clear();
            self.parser = StreamingJsonParser::new();
            self.snapshot = None;
            self.arguments.clear();
            self.append(text)
        }
    }

    pub fn finish(self) -> (Map<String, Value>, Option<String>) {
        parse_tool_call_arguments(Some(&self.text))
    }

    fn partial(&self) -> Map<String, Value> {
        self.arguments.clone()
    }

    fn update_partial(&mut self) -> Map<String, Value> {
        let snapshot = self.parser.snapshot();
        if snapshot == self.snapshot {
            return self.arguments.clone();
        }
        self.snapshot = snapshot.clone();
        self.arguments = match snapshot {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        self.arguments.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_json_fixes_raw_newlines_and_invalid_escapes() {
        // Raw newline and tab in string
        let raw = "{\"text\":\"line1\nline2\twith\x01ctrl\"}";
        let repaired = repair_json(raw);
        assert_eq!(repaired, "{\"text\":\"line1\\nline2\\twith\\u0001ctrl\"}");

        // Invalid escape \d and \url doubled without breaking valid \u0041
        let invalid = r#"{"num":"\d","link":"\url","valid":"\u0041"}"#;
        let repaired2 = repair_json(invalid);
        assert_eq!(
            repaired2,
            r#"{"num":"\\d","link":"\\url","valid":"\u0041"}"#
        );
    }

    #[test]
    fn parse_tool_call_arguments_rejects_non_object_with_ts_message() {
        let (args, err) = parse_tool_call_arguments(Some("[1,2]"));
        assert_eq!(args, Map::new());
        assert_eq!(
            err,
            Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1,2]".to_string())
        );
    }

    #[test]
    fn streaming_parser_emits_partial_objects_across_chunks() {
        let mut stream = ToolCallArgumentsStream::new();
        let step1 = stream.append(r#"{"path":"src/"#);
        assert_eq!(
            step1.get("path").and_then(|v| v.as_str()),
            Some("src/")
        );

        let step2 = stream.append(r#"main.rs","line":"#);
        assert_eq!(
            step2.get("path").and_then(|v| v.as_str()),
            Some("src/main.rs")
        );
        assert!(!step2.contains_key("line"));

        let step3 = stream.append("10}");
        assert_eq!(
            step3.get("path").and_then(|v| v.as_str()),
            Some("src/main.rs")
        );
        assert_eq!(
            step3.get("line").and_then(|v| v.as_i64()),
            Some(10)
        );
    }

    #[test]
    fn parse_tool_call_arguments_handles_empty_and_valid() {
        let (empty_args, empty_err) = parse_tool_call_arguments(None);
        assert_eq!(empty_args, Map::new());
        assert!(empty_err.is_none());

        let (empty_args2, empty_err2) = parse_tool_call_arguments(Some("   "));
        assert_eq!(empty_args2, Map::new());
        assert!(empty_err2.is_none());

        let (args, err) = parse_tool_call_arguments(Some(r#"{"key":"val\ue"}"#));
        assert!(err.is_none());
        assert_eq!(args.get("key").and_then(|v| v.as_str()), Some("val\\ue"));
    }

    #[test]
    fn parse_tool_call_arguments_reports_malformed_syntax() {
        let raw = r#"{"kind":"seek","time":.5}"#;
        let (args, err) = parse_tool_call_arguments(Some(raw));
        assert_eq!(args, Map::new());
        assert!(err.is_some());
        assert!(err.unwrap().ends_with(&format!("\n\nReceived arguments:\n{}", raw)));
    }

    #[test]
    fn streaming_parser_vectors() {
        let mut parser = StreamingJsonParser::new();
        parser.append("\"");
        assert_eq!(parser.snapshot(), Some(Value::String("".to_string())));

        let mut parser = StreamingJsonParser::new();
        parser.append("[\"");
        assert_eq!(parser.snapshot(), Some(serde_json::json!([""])));

        let mut parser = StreamingJsonParser::new();
        parser.append("t");
        assert_eq!(parser.snapshot(), Some(Value::Bool(true)));

        let mut parser = StreamingJsonParser::new();
        parser.append("fals");
        assert_eq!(parser.snapshot(), Some(Value::Bool(false)));

        let mut parser = StreamingJsonParser::new();
        parser.append("nul");
        assert_eq!(parser.snapshot(), Some(Value::Null));

        let mut parser = StreamingJsonParser::new();
        parser.append("12.2");
        assert_eq!(parser.snapshot(), Some(serde_json::json!(12.2)));

        let mut parser = StreamingJsonParser::new();
        parser.append("12.");
        assert_eq!(parser.snapshot(), None);

        let mut parser = StreamingJsonParser::new();
        parser.append("2.5e");
        assert_eq!(parser.snapshot(), Some(serde_json::json!(2.5)));
    }

    #[test]
    fn tool_call_arguments_stream_replace_and_finish() {
        let mut stream = ToolCallArgumentsStream::new();
        stream.append(r#"{"a":"x"#);
        let replaced = stream.replace(r#"{"a":"xyz"}"#);
        assert_eq!(replaced.get("a").and_then(|v| v.as_str()), Some("xyz"));

        let replaced2 = stream.replace(r#"{"b":2}"#);
        assert_eq!(replaced2.get("b").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(stream.text(), r#"{"b":2}"#);

        let (final_args, err) = stream.finish();
        assert!(err.is_none());
        assert_eq!(final_args.get("b").and_then(|v| v.as_i64()), Some(2));
    }
}
