//! Expansion of `@path` prompt arguments into file content and image blocks.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use mehh_ai::{ImageContent, TextContent, UserContentBlock};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessedPromptArgs {
    pub text: String,
    pub blocks: Vec<UserContentBlock>,
}

const MAX_TEXT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

fn read_bounded_file(path: &Path, size_limit: u64, remaining_total: u64) -> Result<Vec<u8>> {
    let effective_limit = size_limit.min(remaining_total);
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open '{}'", path.display()))?;
    let mut bytes = Vec::new();
    file.take(effective_limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to read '{}'", path.display()))?;
    if (bytes.len() as u64) > effective_limit {
        if remaining_total < size_limit {
            anyhow::bail!(
                "File '{}' exceeds the {} byte total attachment limit ({} bytes remaining)",
                path.display(),
                MAX_TOTAL_ATTACHMENT_BYTES,
                remaining_total
            );
        }
        anyhow::bail!(
            "File '{}' exceeds the {} byte limit",
            path.display(),
            size_limit
        );
    }
    Ok(bytes)
}

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

/// Escapes text placed inside the `<file>` body so file content cannot close
/// the wrapper and pass itself off as a new instruction.
fn escape_xml_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;")
}

/// Reads each `@path` argument into a text block or a base64 image block.
/// Non-`@` arguments are joined into the text prompt verbatim.
pub fn process_file_arguments(cwd: &Path, args: &[String]) -> Result<ProcessedPromptArgs> {
    let mut text_parts = Vec::new();
    let mut blocks = Vec::new();
    let mut total_bytes = 0u64;

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
                let remaining_total = MAX_TOTAL_ATTACHMENT_BYTES.saturating_sub(total_bytes);
                if remaining_total == 0 {
                    anyhow::bail!(
                        "Total attachment size exceeds the {} byte limit",
                        MAX_TOTAL_ATTACHMENT_BYTES
                    );
                }
                let bytes = read_bounded_file(&full_path, size_limit, remaining_total)?;
                total_bytes = total_bytes.saturating_add(bytes.len() as u64);

                if is_image {
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
                    if bytes.contains(&0) {
                        anyhow::bail!(
                            "File '{}' appears to be binary; refusing to attach it as text",
                            full_path.display()
                        );
                    }
                    let raw = String::from_utf8_lossy(&bytes);
                    let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
                    let tag = format!(
                        "<file name=\"{}\">\n{}\n</file>",
                        escape_xml_attribute(path_str),
                        escape_xml_text(clean)
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
