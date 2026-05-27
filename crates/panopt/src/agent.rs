//! The `agent` and `_agent` subcommands.

use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::json;

use crate::mcpclient::Client;
use crate::{mcp, paths};

// Note: `_agent` no longer pre-registers against the daemon directly. The
// stdio MCP proxy that Claude Code spawns from this pane (via the config at
// `mcp.rs`) initializes its panoptd session as soon as Claude Code sends
// its first `initialize` over stdio, which both registers the agent and
// applies its name to the registry entry. The pane-death hook keeps using
// `_agent-leave` over HTTP because at that point the agent process is gone
// and there is no proxy to call through.

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
/// the pane *is* the agent with no PANopt wrapper around it. The MCP template
/// at `mcp.rs` tells claude to spawn `panopt _mcp-proxy` as a stdio MCP
/// server, which carries the env vars set here into the panoptd URL and
/// keeps the session alive across daemon restarts.
pub fn exec_in_pane(ws: Option<PathBuf>, id: Option<String>, port: u16) -> Result<()> {
    let config = mcp::ensure()?;
    let ws = resolve_ws(ws)?;
    let id = id.unwrap_or_else(|| mint_id(None));
    let token_path = paths::token()?;
    let token = panopt_core::auth::read_token(&token_path)
        .with_context(|| format!("reading panopt token from {}", token_path.display()))?;
    let host = std::env::var("PANOPT_HOST").unwrap_or_else(|_| "127.0.0.1".into());

    // `exec` replaces this process image, so it returns only on failure.
    let err = Command::new("claude")
        .arg("--mcp-config")
        .arg(&config)
        .env("PANOPT_WS", &ws)
        .env("PANOPT_AGENT", &id)
        .env("PANOPT_NAME", &id)
        .env("PANOPT_PORT", port.to_string())
        .env("PANOPT_HOST", &host)
        .env("PANOPT_TOKEN", &token)
        .exec();
    Err(err).context("could not exec `claude` (is it installed and on PATH?)")
}

/// `panopt _agent-leave --id <id>` - tell the daemon that the agent named by
/// `id` has gone away, so its registry entry and locks clear immediately.
///
/// Used by the sidebar plugin's pane-death hook: when a cockpit-spawned agent
/// pane closes, the plugin runs this command to call the daemon's
/// `agent_leave` MCP tool on behalf of the dead pane. Without it, a closed
/// cockpit-spawned agent (which uses a stable `?agent=<id>` key) would
/// linger in the registry indefinitely because declared identities never
/// idle-prune.
///
/// Connects with `?agent=<id>` so the daemon sees this as a request *from*
/// that agent, and `agent_leave` removes the entry and releases its locks.
/// Best-effort: missing daemon or missing agent are no-ops the caller logs
/// but does not surface.
pub fn leave(ws: Option<PathBuf>, id: String, port: u16) -> Result<()> {
    let ws = resolve_ws(ws)?;
    let token = panopt_core::auth::read_token(&paths::token()?)
        .context("reading the panopt token (start the daemon with `panopt up`)")?;
    let encode = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let url = format!(
        "http://127.0.0.1:{port}/mcp?ws={}&agent={}&token={}",
        encode(&ws.to_string_lossy()),
        encode(&id),
        encode(&token),
    );
    let client = Client::connect(&url).context("connecting to the panopt daemon")?;
    let result = client.call("agent_leave", json!({}));
    client.close();
    result.map(|_| ())
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
