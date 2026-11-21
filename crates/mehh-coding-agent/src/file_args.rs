//! Expansion of `@path` prompt arguments into file content and image blocks.

use std::path::Path;

use anyhow::Result;
use mehh_ai::{ImageContent, TextContent, UserContentBlock};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessedPromptArgs {
    pub text: String,
    pub blocks: Vec<UserContentBlock>,
}

const MAX_TEXT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// Escapes a path for use inside a double-quoted XML attribute so a file name
/// cannot break out of the `<file name="…">` tag.
fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
}

/// Reads each `@path` argument into a text block or a base64 image block.
/// Non-`@` arguments are joined into the text prompt verbatim.
pub fn process_file_arguments(cwd: &Path, args: &[String]) -> Result<ProcessedPromptArgs> {
    let mut text_parts = Vec::new();
    let mut blocks = Vec::new();

    for arg in args {
        if let Some(path_str) = arg.strip_prefix('@') {
            let p = Path::new(path_str);
            let full_path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };

            if full_path.is_file() {
                let ext = full_path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp");
                let size_limit = if is_image {
                    MAX_IMAGE_FILE_BYTES
                } else {
                    MAX_TEXT_FILE_BYTES
                };
                let size = std::fs::metadata(&full_path)?.len();
                if size > size_limit {
                    anyhow::bail!(
                        "File '{}' is {} bytes, exceeding the {} byte limit",
                        full_path.display(),
                        size,
                        size_limit
                    );
                }

                if is_image {
                    let bytes = std::fs::read(&full_path)?;
                    let mime_type = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        _ => "application/octet-stream",
                    };
                    let b64 = crate::clipboard::base64_encode(&bytes);
                    blocks.push(UserContentBlock::Image(ImageContent {
                        data: b64,
                        mime_type: mime_type.to_string(),
                    }));
                    continue;
                } else {
                    let bytes = std::fs::read(&full_path)?;
                    if bytes.contains(&0) {
                        anyhow::bail!(
                            "File '{}' appears to be binary; refusing to attach it as text",
                            full_path.display()
                        );
                    }
                    let raw = String::from_utf8_lossy(&bytes);
                    let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
                    let tag = format!(
                        "<file name=\"{}\">\n{clean}\n</file>",
                        escape_xml_attribute(path_str)
                    );
                    text_parts.push(tag);
                    continue;
                }
            }
        }
        text_parts.push(arg.clone());
    }

    let text = text_parts.join(" ");
    if !text.is_empty() {
        blocks.insert(
            0,
            UserContentBlock::Text(TextContent {
                text: text.clone(),
                text_signature: None,
            }),
        );
    }

    Ok(ProcessedPromptArgs { text, blocks })
}

#[cfg(test)]
mod tests {
    use super::escape_xml_attribute;

    #[test]
    fn escape_xml_attribute_neutralizes_markup_characters() {
        assert_eq!(
            escape_xml_attribute("a\"b&c<d>'e"),
            "a&quot;b&amp;c&lt;d&gt;&apos;e"
        );
    }
}
