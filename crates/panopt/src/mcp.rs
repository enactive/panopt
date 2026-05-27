//! The agent MCP config file.
//!
//! One static, env-templated MCP server entry that every launched agent shares.
//! `claude` expands `${PANOPT_HOST}` / `${PANOPT_PORT}` / `${PANOPT_WS}` /
//! `${PANOPT_AGENT}` / `${PANOPT_NAME}` / `${PANOPT_TOKEN}` from the per-pane
//! environment when it reads the file, so a single file gives each agent a
//! distinct stable identity, a friendly display name, and the bearer token the
//! daemon requires (DESIGN.md Sections 5.3 and 9).

use anyhow::{Context, Result};

use crate::paths;

/// The MCP config the launcher writes for agents to load with `--mcp-config`.
const AGENT_MCP_JSON: &str = r#"{
  "mcpServers": {
    "panopt": {
      "type": "http",
      "url": "http://${PANOPT_HOST:-127.0.0.1}:${PANOPT_PORT:-7600}/mcp?ws=${PANOPT_WS}&agent=${PANOPT_AGENT}&name=${PANOPT_NAME}&token=${PANOPT_TOKEN}"
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
    fn agent_mcp_json_is_valid() {
        let v: serde_json::Value = serde_json::from_str(super::AGENT_MCP_JSON).unwrap();
        assert!(v["mcpServers"]["panopt"]["url"].is_string());
    }
}
