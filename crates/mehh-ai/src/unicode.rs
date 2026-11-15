#[derive(Copy, Clone, PartialEq, Eq)]
enum SurrogateKind {
    High,
    Low,
}

fn parse_hex_u16(b: &[u8]) -> Option<u16> {
    if b.len() != 4 || !b.iter().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let s = std::str::from_utf8(b).ok()?;
    u16::from_str_radix(s, 16).ok()
}

fn surrogate_kind(b: &[u8]) -> Option<SurrogateKind> {
    if b.len() >= 6 && b[0] == b'\\' && (b[1] == b'u' || b[1] == b'U') {
        if let Some(val) = parse_hex_u16(&b[2..6]) {
            if (0xD800..=0xDBFF).contains(&val) {
                return Some(SurrogateKind::High);
            } else if (0xDC00..=0xDFFF).contains(&val) {
                return Some(SurrogateKind::Low);
            }
        }
    }
    None
}

/// Removes unpaired Unicode surrogate characters and NUL bytes from a string.
///
/// Unpaired surrogates (high surrogates `\uD800`..`\uDBFF` without matching low surrogates `\uDC00`..`\uDFFF`,
/// or vice versa, case-insensitive hex) cause JSON serialization or validation errors in API providers.
/// Valid emoji and multi-byte UTF-8, as well as properly paired surrogates, are preserved.
pub fn sanitize_surrogates(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();

    let mut i = 0;
    while i < input.len() {
        if bytes[i] == 0 {
            // Strip NUL byte
            i += 1;
            continue;
        }

        if bytes[i] == b'\\' && i + 6 <= input.len() {
            match surrogate_kind(&bytes[i..i + 6]) {
                Some(SurrogateKind::High) => {
                    // Check if immediately followed by a low surrogate
                    if i + 12 <= input.len()
                        && surrogate_kind(&bytes[i + 6..i + 12]) == Some(SurrogateKind::Low)
                    {
                        out.push_str(&input[i..i + 12]);
                        i += 12;
                    } else {
                        // Unpaired high surrogate: strip
                        i += 6;
                    }
                    continue;
                }
                Some(SurrogateKind::Low) => {
                    // Unpaired low surrogate: strip
                    i += 6;
                    continue;
                }
                None => {}
            }
        }

        let c = input[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }

    out
}

/// Recursively sanitizes string values (and object keys) in a JSON value.
pub fn sanitize_json_surrogates(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = sanitize_surrogates(s);
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                sanitize_json_surrogates(v);
            }
        }
        serde_json::Value::Object(map) => {
            let mut keys_to_update = Vec::new();
            for (k, v) in map.iter_mut() {
                sanitize_json_surrogates(v);
                let sanitized_k = sanitize_surrogates(k);
                if sanitized_k != *k {
                    keys_to_update.push((k.clone(), sanitized_k));
                }
            }
            for (old_k, new_k) in keys_to_update {
                if let Some(v) = map.remove(&old_k) {
                    map.insert(new_k, v);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_surrogates_strips_unpaired_escapes_preserves_valid_pairs() {
        let input = "ok \\uD83D\\uDE48 bad \\uD83D tail \\uDC00 end";
        let cleaned = sanitize_surrogates(input);
        assert_eq!(cleaned, "ok \\uD83D\\uDE48 bad  tail  end");

        let input_with_nul = "Tiếng Việt 🙈\0";
        let cleaned_nul = sanitize_surrogates(input_with_nul);
        assert_eq!(cleaned_nul, "Tiếng Việt 🙈");

        let mut val = serde_json::json!({
            "text": "ok \\uD83D\\uDE48 bad \\uD83D tail \\uDC00 end",
            "nested": ["Tiếng Việt 🙈\0", "\\uDC00orphaned"]
        });
        sanitize_json_surrogates(&mut val);
        assert_eq!(val["text"], "ok \\uD83D\\uDE48 bad  tail  end");
        assert_eq!(val["nested"][0], "Tiếng Việt 🙈");
        assert_eq!(val["nested"][1], "orphaned");
    }

    #[test]
    fn sanitize_surrogates_handles_multibyte_utf8_near_escapes_without_panicking() {
        let input1 = "\\u123\u{1F600}";
        let cleaned1 = sanitize_surrogates(input1);
        assert_eq!(cleaned1, "\\u123\u{1F600}");

        let input2 = "\\u\u{20AC}";
        let cleaned2 = sanitize_surrogates(input2);
        assert_eq!(cleaned2, "\\u\u{20AC}");
    }
}
