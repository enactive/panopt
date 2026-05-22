//! The `agent` and `_agent` subcommands.

use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::mcp;

/// `panopt agent [name]` - open a new agent pane in the running cockpit.
///
/// The cockpit's Zellij plugin is the spawner; this just sends it a
/// `panopt:spawn-agent` pipe message. Must be run inside the cockpit session.
pub fn spawn(name: Option<String>) -> Result<()> {
    if std::env::var_os("ZELLIJ").is_none() {
        bail!("`panopt agent` runs inside the cockpit - start one with `panopt up`");
    }

    let mut cmd = Command::new("zellij");
    cmd.arg("pipe").arg("--name").arg("panopt:spawn-agent");
    if let Some(name) = name {
        // Mint the id here - a native process has the system RNG. The plugin
        // relays the payload straight to `panopt _agent --id`.
        cmd.arg("--").arg(mint_id(Some(&name)));
    }
    let status = cmd
        .status()
        .context("running `zellij pipe` (is zellij installed and on PATH?)")?;
    if !status.success() {
        bail!("`zellij pipe` failed");
    }
    println!("requested a new agent pane");
    Ok(())
}

/// `panopt _agent` - the entrypoint that runs inside an agent pane.
///
/// Sets the per-agent environment and replaces this process with `claude`, so
/// the pane *is* the agent with no PANopt wrapper around it.
pub fn exec_in_pane(ws: Option<PathBuf>, id: Option<String>, port: u16) -> Result<()> {
    let config = mcp::ensure()?;
    let ws = resolve_ws(ws)?;
    let id = id.unwrap_or_else(|| mint_id(None));

    // `exec` replaces this process image, so it returns only on failure.
    let err = Command::new("claude")
        .arg("--mcp-config")
        .arg(&config)
        .env("PANOPT_WS", &ws)
        .env("PANOPT_AGENT", &id)
        .env("PANOPT_PORT", port.to_string())
        .exec();
    Err(err).context("could not exec `claude` (is it installed and on PATH?)")
}

/// The project root: the given path, or the current directory, canonicalized.
fn resolve_ws(ws: Option<PathBuf>) -> Result<PathBuf> {
    let ws = match ws {
        Some(ws) => ws,
        None => std::env::current_dir().context("reading the current directory")?,
    };
    std::fs::canonicalize(&ws).with_context(|| format!("no such directory: {}", ws.display()))
}

/// Mint a stable agent id. With a name: `<name>-<4 hex>` - readable and unique.
/// Without: `agent-<8 hex>`.
fn mint_id(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{name}-{}", hex(2)),
        None => format!("agent-{}", hex(4)),
    }
}

/// `n` random bytes from the system RNG, lowercase hex.
fn hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("reading /dev/urandom");
    let mut out = String::with_capacity(n * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{hex, mint_id};

    #[test]
    fn hex_is_two_chars_per_byte() {
        assert_eq!(hex(4).len(), 8);
        assert!(hex(4).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn named_id_is_readable_and_suffixed() {
        let id = mint_id(Some("backend"));
        assert!(id.starts_with("backend-"), "{id}");
        assert_eq!(id.len(), "backend-".len() + 4);
    }

    #[test]
    fn anonymous_id_is_prefixed() {
        let id = mint_id(None);
        assert!(id.starts_with("agent-"), "{id}");
        assert_eq!(id.len(), "agent-".len() + 8);
    }
}
