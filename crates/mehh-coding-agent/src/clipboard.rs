//! Platform clipboard helper with OSC 52 terminal fallback.

use anyhow::Result;

pub fn base64_encode(input: &[u8]) -> String {
    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
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
    let _ = write!(std::io::stdout(), "\x1b]52;c;{encoded}\x07");
    let _ = std::io::stdout().flush();
    true
}

/// Copies `text` to the system clipboard using platform tools with OSC 52 fallback.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let copied = if cfg!(target_os = "windows") {
        let child = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut c) = child {
            if let Some(mut stdin) = c.stdin.take() {
                use std::io::Write;
                let mut utf16_bytes = vec![0xFF, 0xFE];
                for u in text.encode_utf16() {
                    utf16_bytes.extend_from_slice(&u.to_le_bytes());
                }
                let _ = stdin.write_all(&utf16_bytes);
            }
            c.wait().map(|s| s.success()).unwrap_or(false)
        } else {
            false
        }
    } else if cfg!(target_os = "macos") {
        let child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut c) = child {
            if let Some(mut stdin) = c.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            c.wait().map(|s| s.success()).unwrap_or(false)
        } else {
            false
        }
    } else {
        let commands = [
            ("wl-copy", vec![]),
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ];
        let mut ok = false;
        for (cmd, args) in commands {
            if let Ok(mut c) = std::process::Command::new(cmd)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = c.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(text.as_bytes());
                }
                if c.wait().map(|s| s.success()).unwrap_or(false) {
                    ok = true;
                    break;
                }
            }
        }
        ok
    };

    if copied {
        return Ok(());
    }

    let _ = emit_osc52(text);
    Ok(())
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
}
