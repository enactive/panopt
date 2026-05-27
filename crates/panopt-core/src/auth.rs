//! Bearer-token plumbing for the coordination daemon.
//!
//! The daemon and every panopt client share a single 32-byte (hex-encoded)
//! token written to a 0600 file. [`ensure_token`] reads the existing token or
//! generates and persists a new one atomically; the daemon calls it on
//! startup, callers that only consume the token use [`read_token`].
//!
//! Kept here in `panopt-core` so the transport layer (rmcp + axum + tokio in
//! `panoptd`) and the launcher (`panopt`) share one implementation without
//! reaching across crate boundaries for the file format.

use std::fs;
use std::io::Read;
use std::path::Path;

/// Read the token at `path`, generating one if the file is absent or empty.
/// The new file is created with mode 0600 on Unix so cohabiting processes
/// without the user's uid cannot read it.
///
/// Concurrent first-run callers race on `create_new`; the loser reads the
/// winner's token on its second attempt.
pub fn ensure_token(path: &Path) -> std::io::Result<String> {
    if let Some(token) = try_read(path)? {
        return Ok(token);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token = generate_token()?;
    match write_new(path, &token) {
        Ok(()) => Ok(token),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another process beat us to it - read what they wrote.
            try_read(path)?.ok_or_else(|| {
                std::io::Error::other("token file appeared then vanished during ensure_token")
            })
        }
        Err(e) => Err(e),
    }
}

/// Read the token at `path`. Errors when the file does not exist; callers
/// surface a hint to start the daemon (which creates it).
pub fn read_token(path: &Path) -> std::io::Result<String> {
    try_read(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "no panopt token at {} - start the daemon (`panopt up`) so it generates one",
                path.display()
            ),
        )
    })
}

fn try_read(path: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim();
            Ok(if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 32 random bytes from `/dev/urandom`, lowercase hex (64 chars).
fn generate_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    let mut f = fs::File::open("/dev/urandom")?;
    f.read_exact(&mut bytes)?;
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    Ok(out)
}

#[cfg(unix)]
fn write_new(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_new(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generates_then_reads_back_the_same_token() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token");
        let first = ensure_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        let second = ensure_token(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn read_token_errors_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent");
        assert!(read_token(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("token");
        ensure_token(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
