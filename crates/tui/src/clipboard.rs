use std::io::Write;
use std::sync::Mutex;

static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

pub fn copy_to_clipboard(text: &str) -> bool {
    let clean_text = text.trim_end_matches(['\r', '\n']);
    let mut ok = false;

    if let Ok(mut guard) = CLIPBOARD.lock() {
        if guard.is_none() {
            *guard = arboard::Clipboard::new().ok();
        }

        let mut needs_reinit = false;
        if let Some(clipboard) = guard.as_mut() {
            if clipboard.set_text(clean_text.to_string()).is_ok() {
                ok = true;
                #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
                {
                    use arboard::SetExtLinux;
                    let _ = clipboard
                        .set()
                        .clipboard(arboard::LinuxClipboardKind::Primary)
                        .text(clean_text.to_string());
                }
            } else {
                needs_reinit = true;
            }
        } else {
            needs_reinit = true;
        }

        if needs_reinit {
            if let Ok(mut new_cb) = arboard::Clipboard::new() {
                if new_cb.set_text(clean_text.to_string()).is_ok() {
                    ok = true;
                    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
                    {
                        use arboard::SetExtLinux;
                        let _ = new_cb
                            .set()
                            .clipboard(arboard::LinuxClipboardKind::Primary)
                            .text(clean_text.to_string());
                    }
                }
                *guard = Some(new_cb);
            }
        }
    }

    // OSC 52 for terminal multiplexers, ssh and terminal primary/clipboard paste
    let b64 = base64_encode(clean_text.as_bytes());
    let osc52 = format!("\x1b]52;c;{b64}\x07\x1b]52;p;{b64}\x07");
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52.as_bytes());
    let _ = out.flush();

    ok
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
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
        assert_eq!(base64_encode(b"Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
    }
}
