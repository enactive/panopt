//! Filesystem locations the launcher uses.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// `<config-dir>/panopt/`, created if missing.
fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no per-user config directory found")?
        .join("panopt");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `<data-dir>/panopt/`, created if missing.
fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no per-user data directory found")?
        .join("panopt");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// The shared, env-templated MCP config every agent is launched with.
pub fn mcp_config() -> Result<PathBuf> {
    Ok(config_dir()?.join("agent-mcp.json"))
}

/// Where the launcher writes `panoptd`'s log when it starts the daemon.
pub fn daemon_log() -> Result<PathBuf> {
    Ok(data_dir()?.join("panoptd.log"))
}

/// The shared bearer-token file. The daemon writes it on first boot (0600);
/// every panopt client reads it to authenticate. Path matches the one the
/// daemon resolves at startup so both ends agree without a flag.
pub fn token() -> Result<PathBuf> {
    Ok(data_dir()?.join("token"))
}

/// The cockpit layout `panopt up` generates and hands to Zellij.
pub fn cockpit_layout() -> Result<PathBuf> {
    Ok(config_dir()?.join("cockpit.kdl"))
}

/// The cockpit Zellij config `panopt up` generates: the user's Zellij config
/// with PANopt's keybinding tweaks, handed to Zellij via `--config`.
pub fn cockpit_config() -> Result<PathBuf> {
    Ok(config_dir()?.join("cockpit-config.kdl"))
}

/// The shell script `panopt up` writes so the generated cockpit config can
/// point Zellij's `copy_command` at it. The script reads selection text on
/// stdin and emits OSC 52 to `/dev/tty` - bypassing Zellij's own OSC 52 path
/// which gets eaten somewhere in the WezTerm-via-SSH chain.
pub fn copy_helper_script() -> Result<PathBuf> {
    Ok(data_dir()?.join("copy-to-osc52.sh"))
}
