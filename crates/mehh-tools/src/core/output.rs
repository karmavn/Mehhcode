use std::collections::VecDeque;

/// Largest index <= `idx` that lies on a UTF-8 character boundary.
pub fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Keep at most `max_bytes` bytes from the start of `s`, without splitting a
/// character, and append a notice when anything was dropped.
pub fn truncate_head(s: &str, max_bytes: usize, notice: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = floor_char_boundary(s, max_bytes);
    let mut out = String::with_capacity(cut + notice.len());
    out.push_str(&s[..cut]);
    out.push_str(notice);
    out
}

/// A bounded byte buffer whose head always starts on a UTF-8 character
/// boundary. Used for streaming subprocess output: `Vec::drain(..n)` on raw
/// bytes can split a multi-byte character and produce invalid text.
#[derive(Debug)]
pub struct RollingBytes {
    buf: VecDeque<u8>,
    cap: usize,
}

impl RollingBytes {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    pub fn extend(&mut self, chunk: &[u8]) {
        self.buf.extend(chunk.iter().copied());
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
        // A trimmed head may be a UTF-8 continuation byte; drop leading
        // continuation bytes so the buffer always decodes from a boundary.
        while self.buf.front().is_some_and(|b| b & 0xC0 == 0x80) {
            self.buf.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.to_vec()).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_head_never_splits_a_character() {
        let mut s = "a".repeat(23_999);
        s.push('\u{251c}'); // 3 bytes
        s.push_str("bcdef");
        let out = truncate_head(&s, 24_000, "...[cut]");
        assert!(out.ends_with("...[cut]"));
        let body = out.strip_suffix("...[cut]").unwrap();
        assert!(body.is_char_boundary(body.len()));
        assert_eq!(body.len(), 23_999);
    }

    #[test]
    fn rolling_bytes_head_stays_on_char_boundary() {
        let mut rolling = RollingBytes::new(4);
        rolling.extend("aé".as_bytes()); // a + 2-byte e-acute = 3 bytes
        rolling.extend("é".as_bytes()); // -> 5 bytes, one byte trimmed
        let decoded = rolling.to_string_lossy();
        assert!(!decoded.contains('\u{fffd}'), "unexpected: {decoded:?}");
    }

    #[test]
    fn short_strings_are_not_annotated() {
        assert_eq!(truncate_head("hello", 100, "...[cut]"), "hello");
    }
}
