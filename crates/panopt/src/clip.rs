//! Clipboard helpers for `panopt _viewer`.
//!
//! Two strategies, tried in order by [`copy_to_clipboard`]:
//!
//! 1. **External command** (`pbcopy` / `wl-copy` / `xclip` / `xsel`). Talks
//!    straight to the system clipboard daemon, bypassing Zellij entirely.
//!    This is the strategy that actually works for the common case - Zellij
//!    intercepts OSC 52 and routes it through its own `copy_command`
//!    handler, which is unconfigured in stock setups and so silently drops
//!    the payload.
//! 2. **OSC 52** (`\x1b]52;c;<base64>\x07`). Fallback for the remote /
//!    SSH'd-in case where the local box has no clipboard daemon (or none
//!    that knows the SSH'd-from machine's clipboard). Most modern terminal
//!    emulators forward OSC 52 to the host system clipboard.
//!
//! Order matters because we want the user's machine clipboard to win when
//! they're working locally: external command first, OSC 52 second.

use std::io::{self, Write};
use std::process::{Command, Stdio};

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
/// bytes; they pass through Zellij and the host terminal forwards them to
/// the system clipboard *if* OSC 52 is allowed end-to-end. Zellij's stock
/// behaviour is to intercept the sequence instead of forwarding it - which
/// is why this is a fallback, not the primary path. See
/// [`copy_to_clipboard`] for the actual entry point.
pub(crate) fn emit_osc52(text: &str) -> io::Result<()> {
    let seq = to_osc52(text);
    let mut out = io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Try each candidate clipboard daemon command, in priority order, until one
/// accepts the payload. Returns `true` as soon as a child exits successfully.
///
/// Order: macOS first (`pbcopy`), then Wayland (`wl-copy`), then X11
/// (`xclip` then `xsel`). The `wl-copy` order before X11 matters because
/// systems that run both XWayland and Wayland natively often have both
/// `xclip` and `wl-copy` installed but only the latter actually reaches the
/// running compositor's clipboard.
fn copy_via_external_command(text: &str) -> bool {
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for &(cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        // Take stdin so dropping it closes the pipe and the child sees EOF;
        // without that close `wl-copy --foreground` (and friends) would
        // wait forever.
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                let _ = child.kill();
                continue;
            }
        }
        if let Ok(status) = child.wait() {
            if status.success() {
                return true;
            }
        }
    }
    false
}

/// Put `text` in the system clipboard. Tries the external clipboard daemon
/// commands first (the reliable path inside the cockpit, since Zellij eats
/// OSC 52 by default), then falls back to emitting OSC 52 on stdout for
/// the case where no clipboard daemon is reachable but the terminal can
/// forward OSC 52 to a different machine's clipboard.
pub(crate) fn copy_to_clipboard(text: &str) -> io::Result<()> {
    if copy_via_external_command(text) {
        return Ok(());
    }
    emit_osc52(text)
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
