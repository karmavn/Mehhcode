//! Platform clipboard helper with OSC 52 terminal fallback.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

const CLIPBOARD_CMD_TIMEOUT: Duration = Duration::from_secs(3);

fn run_clipboard_command(
    program: &str,
    args: &[&str],
    input: &[u8],
    timeout: Duration,
) -> Result<()> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to spawn clipboard command '{program}'"))?;

    let write_rx = child.stdin.take().map(|mut stdin| {
        let bytes = input.to_vec();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            use std::io::Write;
            let res = stdin.write_all(&bytes).and_then(|()| stdin.flush());
            drop(stdin);
            let _ = tx.send(res);
        });
        rx
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "Clipboard command '{program}' timed out after {:?}",
                        timeout
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).with_context(|| {
                    format!("Failed waiting for clipboard command '{program}'")
                });
            }
        }
    };

    if let Some(rx) = write_rx {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(e).with_context(|| {
                    format!("Failed writing stdin to clipboard command '{program}'")
                });
            }
            Err(_) => {
                anyhow::bail!("Timed out flushing stdin to clipboard command '{program}'");
            }
        }
    }

    if !status.success() {
        anyhow::bail!("Clipboard command '{program}' exited with status {status}");
    }
    Ok(())
}

pub fn base64_encode(input: &[u8]) -> String {
    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let (chunks, rem) = input.as_chunks::<3>();
    for chunk in chunks {
        let b0 = chunk[0];
        let b1 = chunk[1];
        let b2 = chunk[2];
        out.push(CHARSET[(b0 >> 2) as usize] as char);
        out.push(CHARSET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(CHARSET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        out.push(CHARSET[(b2 & 0x3f) as usize] as char);
    }
    if rem.len() == 1 {
        let b0 = rem[0];
        out.push(CHARSET[(b0 >> 2) as usize] as char);
        out.push(CHARSET[((b0 & 0x03) << 4) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem.len() == 2 {
        let b0 = rem[0];
        let b1 = rem[1];
        out.push(CHARSET[(b0 >> 2) as usize] as char);
        out.push(CHARSET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(CHARSET[((b1 & 0x0f) << 2) as usize] as char);
        out.push('=');
    }
    out
}

/// Emits an OSC 52 clipboard escape sequence to stdout.
pub fn emit_osc52(text: &str) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    let encoded = base64_encode(text.as_bytes());
    if encoded.len() > 100_000 {
        return false;
    }
    use std::io::Write;
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07").is_ok() && stdout.flush().is_ok()
}

/// Copies `text` to the system clipboard using platform tools with OSC 52 fallback.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let platform_res: Result<()> = if cfg!(target_os = "windows") {
        let mut utf16_bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            utf16_bytes.extend_from_slice(&u.to_le_bytes());
        }
        run_clipboard_command("clip", &[], &utf16_bytes, CLIPBOARD_CMD_TIMEOUT)
    } else if cfg!(target_os = "macos") {
        run_clipboard_command("pbcopy", &[], text.as_bytes(), CLIPBOARD_CMD_TIMEOUT)
    } else {
        let commands: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        let mut last_err = None;
        let mut ok = false;
        for &(cmd, args) in commands {
            match run_clipboard_command(cmd, args, text.as_bytes(), CLIPBOARD_CMD_TIMEOUT) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(err) => last_err = Some(err),
            }
        }
        if ok {
            Ok(())
        } else {
            Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No clipboard command succeeded")))
        }
    };

    let err = match platform_res {
        Ok(()) => return Ok(()),
        Err(err) => err,
    };

    if !emit_osc52(text) {
        return Err(err).context(
            "Platform clipboard write failed and OSC 52 fallback unavailable (stdout is not a terminal or payload too large)",
        );
    }
    Ok(())
}

/// Non-blocking async wrapper around [`copy_to_clipboard`] for Tokio callers.
pub async fn copy_to_clipboard_async(text: String) -> Result<()> {
    tokio::task::spawn_blocking(move || copy_to_clipboard(&text))
        .await
        .map_err(|e| anyhow::anyhow!("Clipboard task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_copy_to_clipboard_does_not_panic() {
        let _ = copy_to_clipboard("test clipboard text");
    }

    #[test]
    fn run_clipboard_command_checks_status_and_enforces_timeout() {
        #[cfg(windows)]
        {
            let fail_res =
                run_clipboard_command("cmd", &["/C", "exit 1"], b"hi", Duration::from_secs(2));
            assert!(fail_res.is_err());

            let timeout_res = run_clipboard_command(
                "powershell",
                &["-NoProfile", "-Command", "Start-Sleep -Seconds 5"],
                b"hi",
                Duration::from_millis(100),
            );
            assert!(timeout_res.is_err());
            assert!(timeout_res.unwrap_err().to_string().contains("timed out"));
        }
        #[cfg(unix)]
        {
            let fail_res =
                run_clipboard_command("sh", &["-c", "exit 1"], b"hi", Duration::from_secs(2));
            assert!(fail_res.is_err());

            let timeout_res =
                run_clipboard_command("sh", &["-c", "sleep 5"], b"hi", Duration::from_millis(100));
            assert!(timeout_res.is_err());
            assert!(timeout_res.unwrap_err().to_string().contains("timed out"));
        }
    }
}
