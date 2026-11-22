//! Shared helpers for the `mehh` CLI.

use std::time::Duration;
use mehh_ai::{TextContent, UserContent, UserContentBlock};
use mehh_coding_agent::ProcessedPromptArgs;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::time::{timeout, timeout_at, Instant};

const SUPPLEMENTARY_READ_BUDGET: Duration = Duration::from_secs(5);
const FIRST_READ_IDLE_WINDOW: Duration = Duration::from_millis(500);

/// Reads piped input from an arbitrary buffered reader.
///
/// When `supplementary` is true, stdin is only an addition to a prompt argument,
/// so an idle pipe must not block the run forever. When false, stdin is the
/// prompt itself and the reader is drained until EOF.
pub async fn read_piped_stdin_from<R>(mut reader: R, supplementary: bool) -> anyhow::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();

    if supplementary {
        // A short idle window on the *first* read only decides whether anything
        // is piped at all. Later chunks get one overall budget, so a producer
        // that pauses between writes is not cut off.
        let first = match timeout(FIRST_READ_IDLE_WINDOW, reader.fill_buf()).await {
            Ok(result) => result?,
            Err(_) => return Ok(String::new()),
        };
        if first.is_empty() {
            return Ok(String::new());
        }
        let len = first.len();
        bytes.extend_from_slice(first);
        reader.consume(len);

        let deadline = Instant::now() + SUPPLEMENTARY_READ_BUDGET;
        loop {
            let buf = match timeout_at(deadline, reader.fill_buf()).await {
                Ok(result) => result?,
                Err(_) => break,
            };
            if buf.is_empty() {
                break;
            }
            let len = buf.len();
            bytes.extend_from_slice(buf);
            reader.consume(len);
        }
    } else {
        loop {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                break;
            }
            let len = buf.len();
            bytes.extend_from_slice(buf);
            reader.consume(len);
        }
    }

    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads piped stdin with the policy implied by the prompt arguments.
/// An explicit prompt argument makes stdin supplementary (bounded read);
/// with no argument, stdin is the prompt itself and is drained to EOF.
pub async fn read_piped_stdin_for_prompt<R>(
    reader: R,
    prompt_args: &[String],
) -> anyhow::Result<String>
where
    R: AsyncBufRead + Unpin,
{
    read_piped_stdin_from(reader, !prompt_args.is_empty()).await
}

/// Merges piped stdin into the prompt argument and builds the final content.
/// `process_file_arguments` mirrors the prompt text into the first Text block,
/// so that block must be updated in place; otherwise piped input is lost.
pub fn build_print_user_content(
    processed: ProcessedPromptArgs,
    piped: &str,
) -> anyhow::Result<UserContent> {
    let mut prompt_text = processed.text;
    let mut blocks = processed.blocks;

    let piped = piped.trim_start_matches('\u{feff}');
    if !piped.trim().is_empty() {
        if !prompt_text.trim().is_empty() {
            prompt_text.push_str("\n\n");
        }
        prompt_text.push_str(piped);
    }

    if prompt_text.trim().is_empty() && blocks.is_empty() {
        anyhow::bail!(
            "Prompt is required. Pass a prompt argument, pipe input via stdin, or use --mode rpc."
        );
    }

    if blocks.is_empty() {
        return Ok(UserContent::Text(prompt_text));
    }

    if !prompt_text.trim().is_empty() {
        match blocks
            .iter_mut()
            .find(|b| matches!(b, UserContentBlock::Text(_)))
        {
            Some(UserContentBlock::Text(text)) => text.text = prompt_text,
            _ => blocks.insert(
                0,
                UserContentBlock::Text(TextContent {
                    text: prompt_text,
                    text_signature: None,
                }),
            ),
        }
    }

    Ok(UserContent::Blocks(blocks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn supplementary_read_keeps_chunk_arriving_after_idle_gap() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let producer = tokio::spawn(async move {
            writer.write_all(b"A\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = writer.write_all(b"B\n").await;
        });

        let output = read_piped_stdin_from(BufReader::new(reader), true)
            .await
            .unwrap();
        producer.await.unwrap();

        assert_eq!(output, "A\nB\n");
    }

    #[tokio::test]
    async fn open_idle_pipe_does_not_block_when_prompt_argument_present() {
        // Keep the writer alive: EOF never arrives. With a prompt argument,
        // stdin is supplementary and must stop after the first idle window.
        let (_writer, reader) = tokio::io::duplex(64);
        let outcome = timeout(
            Duration::from_millis(900),
            read_piped_stdin_for_prompt(BufReader::new(reader), &["hello".to_string()]),
        )
        .await;
        assert!(
            matches!(outcome, Ok(Ok(ref text)) if text.is_empty()),
            "stdin must not be drained to EOF when a prompt argument exists: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn slow_first_byte_is_waited_for_when_stdin_is_the_prompt() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(650)).await;
            writer.write_all(b"review this").await.unwrap();
        });

        let output = read_piped_stdin_for_prompt(BufReader::new(reader), &[])
            .await
            .unwrap();
        producer.await.unwrap();

        assert_eq!(
            output, "review this",
            "stdin is the prompt, so a slow first byte must not hit the idle window"
        );
    }

    #[test]
    fn build_print_user_content_merges_piped_stdin_with_prompt_and_at_file() {
        let dir = std::env::temp_dir().join(format!(
            "mehh_merge_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "FILE_BODY").unwrap();

        let processed = mehh_coding_agent::process_file_arguments(
            &dir,
            &["review".to_string(), "@a.txt".to_string()],
        )
        .unwrap();
        let content = build_print_user_content(processed, "\u{feff}DIFF_BODY").unwrap();

        let blocks = match content {
            mehh_ai::UserContent::Blocks(blocks) => blocks,
            other => panic!("expected blocks, got {other:?}"),
        };
        let text = blocks
            .iter()
            .find_map(|b| match b {
                mehh_ai::UserContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .expect("a text block must exist");

        assert!(text.contains("review"), "prompt argument lost: {text}");
        assert!(text.contains("FILE_BODY"), "@file content lost: {text}");
        assert!(
            text.contains("DIFF_BODY"),
            "piped stdin lost when a prompt argument is present: {text}"
        );
        assert!(!text.contains('\u{feff}'), "BOM must be stripped: {text}");

        let _ = std::fs::remove_dir_all(dir);
    }
}
