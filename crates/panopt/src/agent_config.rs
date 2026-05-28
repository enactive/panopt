//! `panopt agent-config` - emit a Claude Code `--mcp-config` JSON for a
//! hand-launched agent.
//!
//! Bridges the gap between cockpit-spawned agents (which get a stable id and
//! identity via the env-templated config at `mcp.rs`) and agents started
//! manually with `claude --mcp-config ...`. Without this, a hand-launched
//! agent falls back to the rotating MCP session id and shows up in the
//! registry as a ghost that other agents cannot pin down (DESIGN.md §9).
//!
//! Emits a stdio config so Claude Code spawns `panopt _mcp-proxy`, which
//! holds the long-lived MCP session and forwards to panoptd over HTTP -
//! making the hand-launched session survive panoptd restarts transparently,
//! the same way cockpit-spawned ones do.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;

use crate::paths;

/// Emit the `--mcp-config` JSON to stdout.
///
/// `token`, when supplied, replaces the usual read of
/// `~/.local/share/panopt/token`. This is the seam that lets an agent host
/// on another machine emit a config without first scp'ing the daemon
/// host's token file into place: paste the value from `panopt token` on
/// the daemon host directly.
pub fn run(
    host: Option<String>,
    port: u16,
    id: Option<String>,
    name: Option<String>,
    token: Option<String>,
    ws: Option<PathBuf>,
) -> Result<()> {
    let host = host.unwrap_or_else(|| "127.0.0.1".into());
    let id = id.unwrap_or_else(default_id);
    let name = name.unwrap_or_else(|| id.clone());
    let ws = resolve_ws(ws)?;
    let token = match token {
        Some(t) => t,
        None => panopt_core::auth::read_token(&paths::token()?).context(
            "reading the panopt token (pass --token <value>, or start the daemon with \
             `panopt up`)",
        )?,
    };
    // Bake in this binary's absolute path so the spawned proxy is the same
    // panopt the user invoked, regardless of what `claude`'s PATH looks like
    // when it executes the stdio child. Falls back to bare `panopt` only if
    // the OS refuses to tell us our own path - in which case PATH lookup is
    // the best we can do.
    let panopt_bin = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "panopt".to_string());

    let cfg = json!({
        "mcpServers": {
            "panopt": {
                "type": "stdio",
                "command": panopt_bin,
                "args": [
                    "--port", port.to_string(),
                    "_mcp-proxy",
                    "--host", host,
                    "--ws", ws.to_string_lossy(),
                    "--id", id,
                    "--name", name,
                    "--token", token,
                ],
            }
        }
    });
    println!("{}", serde_json::to_string_pretty(&cfg)?);
    Ok(())
}

/// `<user>-<hostname>` - a stable per-machine id with enough context that
/// other agents can tell who they're looking at. Falls back to "agent" if
/// neither USER nor /etc/hostname is available.
fn default_id() -> String {
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "agent".to_string());
    let host = hostname().unwrap_or_else(|| "host".to_string());
    format!("{user}-{host}")
}

/// The local hostname (trimmed) from `/etc/hostname`, or `None` if the file
/// is absent or unreadable.
fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The project root: the given path, or the current directory, canonicalized.
fn resolve_ws(ws: Option<PathBuf>) -> Result<PathBuf> {
    let ws = match ws {
        Some(ws) => ws,
        None => std::env::current_dir().context("reading the current directory")?,
    };
    std::fs::canonicalize(&ws).with_context(|| format!("no such directory: {}", ws.display()))
}

#[cfg(test)]
mod tests {
    use super::default_id;

    #[test]
    fn default_id_includes_user_or_falls_back() {
        // Just exercise the function - the result depends on the env. It
        // must always produce a non-empty, hyphenated string.
        let id = default_id();
        assert!(id.contains('-'), "expected user-host shape: {id}");
        assert!(!id.starts_with('-') && !id.ends_with('-'), "{id}");
    }
}
