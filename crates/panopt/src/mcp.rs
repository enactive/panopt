//! The agent MCP config file.
//!
//! One static, env-templated MCP server entry that every launched agent shares:
//! `claude` expands `${PANOPT_WS}` / `${PANOPT_AGENT}` / `${PANOPT_PORT}` from
//! the per-pane environment when it reads the file, so a single file gives each
//! agent a distinct, stable identity (DESIGN.md Sections 5.3 and 9).

use anyhow::{Context, Result};

use crate::paths;

/// The MCP config the launcher writes for agents to load with `--mcp-config`.
const AGENT_MCP_JSON: &str = r#"{
  "mcpServers": {
    "panopt": {
      "type": "http",
      "url": "http://127.0.0.1:${PANOPT_PORT:-7600}/mcp?ws=${PANOPT_WS}&agent=${PANOPT_AGENT}"
    }
  }
}
"#;

/// Ensure the agent MCP config exists and return its path. Written only when
/// absent, so a hand-edited file is left untouched.
pub fn ensure() -> Result<std::path::PathBuf> {
    let path = paths::mcp_config()?;
    if !path.exists() {
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
