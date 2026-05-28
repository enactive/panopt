//! The agent MCP config file.
//!
//! One static, env-templated MCP server entry that every launched agent shares.
//! Claude Code spawns `panopt _mcp-proxy` as a stdio MCP server; the proxy
//! then forwards everything to panoptd over HTTP and reconnects across
//! daemon restarts so Claude Code's session stays up. `claude` expands
//! `${PANOPT_BIN}` / `${PANOPT_HOST}` / `${PANOPT_PORT}` / `${PANOPT_WS}` /
//! `${PANOPT_AGENT}` / `${PANOPT_NAME}` / `${PANOPT_TOKEN}` from the
//! per-pane environment when it reads the file, so a single file gives
//! each agent a distinct stable identity, a friendly display name, and
//! the bearer token the daemon requires (DESIGN.md Sections 5.3 and 9).
//!
//! `${PANOPT_BIN}` is the absolute path of the running launcher, set in
//! `agent::exec_in_pane`. Baking in the absolute path means the spawned
//! proxy is the exact panopt binary the user is running, regardless of
//! what claude's PATH looks like - this matters because `cargo run` and
//! `cargo install` land in different places, and claude inherits whichever
//! environment its parent had.

use anyhow::{Context, Result};

use crate::paths;

/// The MCP config the launcher writes for agents to load with `--mcp-config`.
const AGENT_MCP_JSON: &str = r#"{
  "mcpServers": {
    "panopt": {
      "type": "stdio",
      "command": "${PANOPT_BIN:-panopt}",
      "args": [
        "--port", "${PANOPT_PORT:-7600}",
        "_mcp-proxy",
        "--host", "${PANOPT_HOST:-127.0.0.1}",
        "--ws", "${PANOPT_WS}",
        "--id", "${PANOPT_AGENT}",
        "--name", "${PANOPT_NAME}",
        "--token", "${PANOPT_TOKEN}"
      ]
    }
  }
}
"#;

/// Ensure the agent MCP config matches the current launcher template and
/// return its path. Rewritten whenever the on-disk file does not match the
/// current `AGENT_MCP_JSON` so token/identity placeholders added in newer
/// launcher releases pick up automatically. Users who want a custom MCP
/// surface should hand `claude` their own `--mcp-config` instead of editing
/// this file.
pub fn ensure() -> Result<std::path::PathBuf> {
    let path = paths::mcp_config()?;
    let matches_template = std::fs::read_to_string(&path)
        .map(|existing| existing == AGENT_MCP_JSON)
        .unwrap_or(false);
    if !matches_template {
        std::fs::write(&path, AGENT_MCP_JSON)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_mcp_json_is_valid_stdio_config() {
        let v: serde_json::Value = serde_json::from_str(super::AGENT_MCP_JSON).unwrap();
        let server = &v["mcpServers"]["panopt"];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["command"], "panopt");
        let args = server["args"].as_array().expect("args is an array");
        // The proxy subcommand has to be present, and every templated env
        // var the proxy needs has to make it into the args list.
        let joined: String = args
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("_mcp-proxy"), "{joined}");
        for needle in [
            "${PANOPT_PORT",
            "${PANOPT_HOST",
            "${PANOPT_WS}",
            "${PANOPT_AGENT}",
            "${PANOPT_NAME}",
            "${PANOPT_TOKEN}",
        ] {
            assert!(joined.contains(needle), "missing {needle} in {joined}");
        }
    }
}
