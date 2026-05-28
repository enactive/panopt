//! OSC 52 clipboard helper.
//!
//! OSC 52 is the terminal escape sequence "store this base64 payload in the
//! system clipboard": `\x1b]52;c;<base64>\x07`. Every major terminal emulator
//! the cockpit might run under honours it (iTerm2, Terminal.app, kitty,
//! wezterm, alacritty, foot), and Zellij passes it through to the host
//! terminal. That means the form's drag-to-copy gesture can reach the user's
//! real clipboard without a clipboard daemon (xclip / wl-copy / ...) or a
//! GUI dependency at all.

use std::io::{self, Write};

/// Encode `bytes` in the standard `A-Z a-z 0-9 + /` base64 alphabet with `=`
/// padding. Hand-rolled rather than pulled in as a crate dependency; OSC 52
/// is the only use site and the encoder is twenty lines.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() >= 2 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() >= 3 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Build the OSC 52 escape sequence carrying `text`. The `c` selector targets
/// the system clipboard (as opposed to a primary / secondary buffer); the
/// `\x07` terminator is the form every terminal emulator we care about
/// accepts (older docs also show `\x1b\\` ST, but `\x07` BEL is universal).
pub(crate) fn to_osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Write the OSC 52 sequence for `text` straight to stdout. The viewer holds
/// the terminal in raw alt-screen mode, so crossterm does not intercept the
/// bytes; they pass through Zellij to the host terminal, which writes them
/// into the system clipboard.
pub(crate) fn emit_osc52(text: &str) -> io::Result<()> {
    let seq = to_osc52(text);
    let mut out = io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, to_osc52};

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        // From RFC 4648 §10.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn to_osc52_wraps_payload_in_the_escape_sequence() {
        let seq = to_osc52("hello");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        // The body is the base64 of `hello`.
        assert!(seq.contains("aGVsbG8="));
    }
}
