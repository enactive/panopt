//! Clipboard helpers for `panopt _viewer`.
//!
//! Three strategies, tried in order by [`copy_to_clipboard`]:
//!
//! 1. **Zellij plugin pipe**. Inside the cockpit we ship the text to the
//!    `panopt-zellij` plugin via `zellij action pipe -- <text>`; the
//!    plugin then calls Zellij's host `copy_to_clipboard` API. The
//!    plugin holds the `WriteToClipboard` permission (pre-granted by
//!    `up::ensure_clipboard_permission_granted`), so Zellij does whatever
//!    the user's clipboard config asks - including emitting OSC 52 to the
//!    host terminal end-to-end with no intermediate layer eating it.
//!    This is the path that actually works for the SSH'd-into-a-host case.
//! 2. **External command** (`pbcopy` / `wl-copy` / `xclip` / `xsel`).
//!    Useful when we're not inside Zellij (a stand-alone `panopt _viewer`
//!    for tests / hacks) and a local clipboard daemon is reachable.
//! 3. **OSC 52** (`\x1b]52;c;<base64>\x07`), raw to stdout, with tmux
//!    DCS pass-through when `$TMUX` is set. Last-resort fallback.

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

/// Wrap an OSC 52 sequence (or any escape sequence) in tmux's DCS
/// pass-through envelope when we appear to be running inside tmux. The
/// wrapper is `ESC P tmux; ESC <payload> ESC \\`; tmux strips the leading
/// `ESC P tmux;` and trailing `ESC \\`, doubles the inner `ESC` back to a
/// single one, and forwards the rest to its own parent terminal. Requires
/// `set -g allow-passthrough on` in the user's tmux config (off by default
/// in tmux 3.3+).
///
/// Detection uses `$TMUX`, which tmux exports for every process it spawns.
/// Outside tmux the payload is returned unchanged.
fn wrap_for_tmux(payload: &str) -> String {
    if std::env::var_os("TMUX").is_none() {
        return payload.to_string();
    }
    // Inside tmux the payload's own ESCs would be eaten when tmux strips
    // its DCS wrapper; double them so one survives the strip.
    let escaped = payload.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// Write the OSC 52 sequence for `text` straight to stdout. The viewer holds
/// the terminal in raw alt-screen mode, so crossterm does not intercept the
/// bytes; they pass through Zellij and the host terminal forwards them to
/// the system clipboard *if* OSC 52 is allowed end-to-end. Zellij's stock
/// behaviour is to intercept the sequence instead of forwarding it - which
/// is why this is a fallback, not the primary path. See
/// [`copy_to_clipboard`] for the actual entry point.
///
/// When `$TMUX` is set the sequence is also wrapped in a DCS pass-through
/// envelope so tmux forwards it up to the next layer (Zellij forwards DCS
/// strings by default, and tmux strips the wrapper on its way out). Users
/// who want this chain to work need `set -g allow-passthrough on` in their
/// tmux config; otherwise tmux drops the DCS pass-through.
pub(crate) fn emit_osc52(text: &str) -> io::Result<()> {
    let seq = wrap_for_tmux(&to_osc52(text));
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

/// Ship the text to the `panopt-zellij` plugin via `zellij action pipe`.
/// The plugin then calls Zellij's host `copy_to_clipboard` API, which
/// respects the user's `copy_command` / `copy_clipboard` config and emits
/// OSC 52 to the parent terminal where appropriate - Zellij's
/// cross-layer clipboard story, end-to-end.
///
/// Skipped (returns false) when `$ZELLIJ` is unset; we are running outside
/// the cockpit and the spawn would fail anyway.
fn copy_via_zellij_plugin(text: &str) -> bool {
    if std::env::var_os("ZELLIJ").is_none() {
        return false;
    }
    let Ok(status) = Command::new("zellij")
        .args([
            "action",
            "pipe",
            "--name",
            "panopt:copy-to-clipboard",
            "--plugin-configuration",
            "mode=todos",
            "--",
            text,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    else {
        return false;
    };
    status.success()
}

/// Put `text` in the system clipboard. Tries every path that might work,
/// not just the first one that returns success - the failure modes of each
/// path are silent (a clipboard daemon may exit 0 without actually writing
/// to a session-less compositor; a Zellij pipe spawn may succeed without
/// the plugin's host call reaching the host terminal). The user's
/// clipboard ends up with the text as soon as *any* path actually lands
/// it, so running every path is cheaper than diagnosing which one is
/// silently broken in their setup.
pub(crate) fn copy_to_clipboard(text: &str) -> io::Result<()> {
    log_copy_request(text);
    // Cheapest path first: a direct OSC 52 to our own stdout. Zellij sees
    // the bytes and decides whether to forward them to the host terminal
    // or eat them; users with a working OSC-52 chain (most modern setups)
    // get their clipboard here. Wrapped in tmux DCS pass-through when
    // `$TMUX` is set.
    let osc52_result = emit_osc52(text);
    // Plugin pipe path: ships the text to `panopt-zellij`'s pipe handler,
    // which calls Zellij's host `copy_to_clipboard` (separate code path
    // from raw OSC-52 forwarding in Zellij).
    let _ = copy_via_zellij_plugin(text);
    // External daemon for the non-Zellij dev cases.
    let _ = copy_via_external_command(text);
    osc52_result
}

/// Append a diagnostic line to the cockpit's copy-debug log alongside the
/// helper script's own log entries, so a mismatch between "what the viewer
/// asked to copy" and "what the script saw on stdin" points at the
/// corruption layer. Silent on any write failure (debug, not load-bearing).
fn log_copy_request(text: &str) {
    let Some(path) = crate::paths::copy_debug_log().ok() else {
        return;
    };
    let line = format!(
        "=== viewer copy_to_clipboard pid={pid} bytes={bytes} ===\nviewer body: {head:?}\n",
        pid = std::process::id(),
        bytes = text.len(),
        head = &text.chars().take(200).collect::<String>(),
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
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

    /// `wrap_for_tmux` is a no-op when `$TMUX` is unset; doubles ESCs and
    /// wraps in `ESC P tmux; ... ESC \\` when set. Lock the env var for the
    /// duration of the test - other tests in the suite may run in parallel
    /// but they do not touch `$TMUX`, so we use a manual save/restore.
    #[test]
    fn wrap_for_tmux_round_trip() {
        use super::wrap_for_tmux;
        let saved = std::env::var_os("TMUX");
        // SAFETY: tests in this binary are single-threaded for env access by
        // convention; no other test in this crate touches `$TMUX`.
        unsafe {
            std::env::remove_var("TMUX");
        }
        assert_eq!(wrap_for_tmux("\x1b]52;c;ZA==\x07"), "\x1b]52;c;ZA==\x07");
        unsafe {
            std::env::set_var("TMUX", "/tmp/tmux-1000/default,123,0");
        }
        let wrapped = wrap_for_tmux("\x1b]52;c;ZA==\x07");
        // ESC at start of OSC was doubled, wrapper has its own bracketing ESC.
        assert!(wrapped.starts_with("\x1bPtmux;\x1b\x1b]52;c;"));
        assert!(wrapped.ends_with("\x07\x1b\\"));
        // Restore.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("TMUX", v),
                None => std::env::remove_var("TMUX"),
            }
        }
    }
}
