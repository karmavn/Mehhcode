use std::borrow::Cow;

/// Strips NUL (`\0`) bytes from a string, borrowing when no NUL byte is present.
pub fn sanitize_surrogates_cow(input: &str) -> Cow<'_, str> {
    if !input.as_bytes().contains(&0) {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(input.replace('\0', ""))
    }
}

/// Strips NUL (`\0`) bytes from a string while preserving all valid UTF-8 and literal `\uD800`..`\uDFFF` text verbatim.
///
/// Rust `&str` is guaranteed to be valid UTF-8 and cannot contain raw UTF-16 surrogate code units.
pub fn sanitize_surrogates(input: &str) -> String {
    sanitize_surrogates_cow(input).into_owned()
}

/// Recursively sanitizes string values (and object keys) in a JSON value.
pub fn sanitize_json_surrogates(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.as_bytes().contains(&0) {
                *s = s.replace('\0', "");
            }
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
                if k.as_bytes().contains(&0) {
                    keys_to_update.push((k.clone(), k.replace('\0', "")));
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
    fn sanitize_surrogates_preserves_literal_escapes_and_strips_nul() {
        let input = "ok \\uD83D\\uDE48 bad \\uD83D tail \\uDC00 end \\\\uD800";
        let cleaned = sanitize_surrogates(input);
        assert_eq!(cleaned, input);

        let input_with_nul = "Ti\u{1EBF}ng Vi\u{1EC7}t \u{1F648}\0";
        let cleaned_nul = sanitize_surrogates(input_with_nul);
        assert_eq!(cleaned_nul, "Ti\u{1EBF}ng Vi\u{1EC7}t \u{1F648}");

        let mut val = serde_json::json!({
            "text": "ok \\uD83D\\uDE48 bad \\uD83D tail \\uDC00 end",
            "nested": ["Ti\u{1EBF}ng Vi\u{1EC7}t \u{1F648}\0", "\\uDC00orphaned"]
        });
        sanitize_json_surrogates(&mut val);
        assert_eq!(val["text"], "ok \\uD83D\\uDE48 bad \\uD83D tail \\uDC00 end");
        assert_eq!(val["nested"][0], "Ti\u{1EBF}ng Vi\u{1EC7}t \u{1F648}");
        assert_eq!(val["nested"][1], "\\uDC00orphaned");
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
