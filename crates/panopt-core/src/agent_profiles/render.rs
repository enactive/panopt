//! The spawn interpreter: render a [`AgentProfile`]'s spawn template into a
//! concrete process launch.
//!
//! This is the launch half of "a type is data, not code". One generic engine
//! turns any profile's `(argv, env, files)` template into a real
//! `(argv, env, written-files)` launch by substituting [`Facts`] - the facts
//! PANopt supplies - for the `{{placeholder}}`s. There is no per-type branch:
//! env-vs-flag-vs-config-file is just *where* a placeholder lands.
//!
//! Substitution is two-pass because a `{{file:NAME}}` resolves to the *path* of
//! a materialized file, which is not known until the file is written:
//!
//! 1. Render and write each `files` entry to a temp file under `dir`, recording
//!    its path.
//! 2. Render `argv` (then `default_args`) and `env`, resolving `{{file:NAME}}`
//!    to the path written in pass 1.
//!
//! [`Facts`] is the other half of the [`super::KNOWN_PLACEHOLDERS`] contract:
//! one value per known name. Gathering those values from the live environment
//! (the daemon's host/port, the config's id/name, the token file, this binary's
//! path) needs launcher/daemon context and so happens at spawn time (#141);
//! this module is the pure engine those callers feed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::AgentProfile;

/// The values substituted for `{{placeholder}}`s. One field per
/// [`super::KNOWN_PLACEHOLDERS`] entry; [`Facts::lookup`] is the mapping the
/// renderer uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Absolute path of the running launcher, so the spawned proxy is the same
    /// panopt binary regardless of the agent's `PATH`.
    pub panopt_bin: String,
    /// Host panoptd listens on.
    pub host: String,
    /// Port panoptd listens on.
    pub port: u16,
    /// The project's workspace path (absolute).
    pub ws: String,
    /// The project identity key.
    pub project: String,
    /// The agent's stable id.
    pub agent_id: String,
    /// The agent's friendly display name.
    pub name: String,
    /// The daemon's bearer token.
    pub token: String,
    /// The model the config pins, if any. `{{model}}` renders empty when unset.
    pub model: Option<String>,
    /// The spawned instance's numeric id (todo #159). `None` renders
    /// `{{process_id}}` empty - the case for any spawn template evaluated before
    /// a row exists; the instructions renderer always sets it.
    pub process_id: Option<u64>,
}

impl Facts {
    /// Resolve a (non-`file:`) placeholder name to its value, or `None` if the
    /// name is not a known fact. Mirrors [`super::KNOWN_PLACEHOLDERS`] exactly.
    fn lookup(&self, name: &str) -> Option<String> {
        Some(match name {
            "panopt_bin" => self.panopt_bin.clone(),
            "host" => self.host.clone(),
            "port" => self.port.to_string(),
            "ws" => self.ws.clone(),
            "project" => self.project.clone(),
            "agent_id" => self.agent_id.clone(),
            "name" => self.name.clone(),
            "token" => self.token.clone(),
            "model" => self.model.clone().unwrap_or_default(),
            "process_id" => self.process_id.map(|n| n.to_string()).unwrap_or_default(),
            _ => return None,
        })
    }
}

/// A rendered, ready-to-spawn launch. The caller (#141) spawns `argv` directly
/// with `env` set, then is responsible for `files`' lifetime: the agent reads
/// them at startup, so they must outlive the spawn, and the caller removes them
/// when the instance exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The fully-resolved argument vector. `argv[0]` is the program to exec.
    pub argv: Vec<String>,
    /// Environment variables to set on the child.
    pub env: BTreeMap<String, String>,
    /// Paths of the temp files written under the caller's `dir`, for cleanup.
    pub files: Vec<PathBuf>,
}

/// Render `profile`'s spawn template against `facts`, materializing any `files`
/// under `dir`. Writes files (I/O) but does not spawn - the engine the
/// lifecycle layer drives. `dir` should be a per-instance temp directory the
/// caller owns and cleans up.
pub fn build_launch(
    profile: &AgentProfile,
    facts: &Facts,
    dir: &Path,
) -> Result<Launch, RenderError> {
    // Pass 1: render each file's contents (facts only) and write it, recording
    // name -> path so pass 2 can resolve `{{file:NAME}}`.
    let mut file_paths: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut written = Vec::new();
    for (name, template) in &profile.spawn.files {
        let contents = substitute(template, &|n| resolve(n, facts, &file_paths))?;
        let path = dir.join(sanitize(name));
        std::fs::write(&path, contents)?;
        file_paths.insert(name.clone(), path.clone());
        written.push(path);
    }

    // Pass 2: render argv (+ the type's standing default_args) and env, now
    // that file paths exist.
    let mut argv = Vec::with_capacity(profile.spawn.argv.len() + profile.default_args.len());
    for template in profile.spawn.argv.iter().chain(&profile.default_args) {
        argv.push(substitute(template, &|n| resolve(n, facts, &file_paths))?);
    }
    let mut env = BTreeMap::new();
    for (key, template) in &profile.spawn.env {
        env.insert(
            key.clone(),
            substitute(template, &|n| resolve(n, facts, &file_paths))?,
        );
    }

    Ok(Launch {
        argv,
        env,
        files: written,
    })
}

/// Render `profile`'s `instructions` template against `facts` (todo #159), or
/// `Ok(None)` if the type declares no instructions. Unlike [`build_launch`] this
/// writes no files and resolves only facts (`{{file:NAME}}` is rejected at load
/// for instructions), so it is a pure string render the daemon runs at spawn
/// time to hand an orchestrator the child's bootstrap text.
pub fn render_instructions(
    profile: &AgentProfile,
    facts: &Facts,
) -> Result<Option<String>, RenderError> {
    let Some(template) = &profile.instructions else {
        return Ok(None);
    };
    let rendered = substitute(template, &|name| {
        if name.starts_with("file:") {
            return Err(RenderError::UnknownFile(name.to_string()));
        }
        facts
            .lookup(name)
            .ok_or_else(|| RenderError::UnknownPlaceholder(name.to_string()))
    })?;
    Ok(Some(rendered))
}

/// Resolve one placeholder name: `file:NAME` -> the materialized path, anything
/// else -> a fact. Both arms error rather than silently emptying, so a bad
/// profile that slipped past load validation still fails loudly.
fn resolve(
    name: &str,
    facts: &Facts,
    file_paths: &BTreeMap<String, PathBuf>,
) -> Result<String, RenderError> {
    match name.strip_prefix("file:") {
        Some(file) => file_paths
            .get(file)
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| RenderError::UnknownFile(file.to_string())),
        None => facts
            .lookup(name)
            .ok_or_else(|| RenderError::UnknownPlaceholder(name.to_string())),
    }
}

/// Substitute every `{{name}}` in `template` using `resolve`. An unterminated
/// `{{` is an error (the load validator's scanner stops at it silently; here we
/// are producing output, so a dangling open brace must not be emitted raw).
fn substitute(
    template: &str,
    resolve: &dyn Fn(&str) -> Result<String, RenderError>,
) -> Result<String, RenderError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let close = after.find("}}").ok_or(RenderError::Unterminated)?;
        out.push_str(&resolve(after[..close].trim())?);
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Reduce a `files` key to a safe single path segment, so a profile (or
/// override) cannot escape the caller's temp dir via `../` or a slash.
fn sanitize(name: &str) -> String {
    // Note: `.` is deliberately *not* allowed, so a bare `..` cannot survive as
    // a path segment. The agent is handed the file path explicitly, so a lost
    // extension is harmless.
    name.replace(
        |c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-',
        "_",
    )
}

/// Errors from rendering a spawn template.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// A `{{` with no closing `}}`.
    #[error("unterminated '{{{{' in spawn template")]
    Unterminated,

    /// A placeholder that is not a known fact. Defensive: load validation
    /// (`ProfileSet::validate`) should have rejected this already.
    #[error("spawn template references unknown placeholder '{0}'")]
    UnknownPlaceholder(String),

    /// A `{{file:NAME}}` whose `NAME` is not a declared file.
    #[error("spawn template references undeclared file '{0}'")]
    UnknownFile(String),

    /// A spawn file could not be written.
    #[error("writing spawn file: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_profiles::ProfileSet;

    fn sample_facts() -> Facts {
        Facts {
            panopt_bin: "/abs/panopt".into(),
            host: "127.0.0.1".into(),
            port: 7600,
            ws: "/home/u/proj".into(),
            project: "proj-key".into(),
            agent_id: "u-host".into(),
            name: "greg-main".into(),
            token: "secret-token".into(),
            model: None,
            process_id: None,
        }
    }

    #[test]
    fn claude_code_renders_to_the_cockpit_launch() {
        let set = ProfileSet::load().expect("load");
        let profile = set.get("claude-code").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let launch = build_launch(profile, &sample_facts(), dir.path()).expect("render");

        // argv: claude --mcp-config <materialized path> --disallowedTools Agent
        //       --append-system-prompt <directive>. The Agent/Task guard steers
        // spawns through panopt's spawn_agent (todo #190 follow-up).
        assert_eq!(launch.argv.len(), 7);
        assert_eq!(launch.argv[0], "claude");
        assert_eq!(launch.argv[1], "--mcp-config");
        assert_eq!(launch.files.len(), 1);
        assert_eq!(launch.argv[2], launch.files[0].to_string_lossy());
        assert_eq!(launch.argv[3], "--disallowedTools");
        assert_eq!(launch.argv[4], "Agent");
        assert_eq!(launch.argv[5], "--append-system-prompt");
        assert!(launch.argv[6].contains("spawn_agent"));
        assert!(launch.env.is_empty());

        // The materialized mcp config has every fact substituted and boots the
        // stdio proxy - the same shape the cockpit hand-builds in mcp.rs.
        let cfg = std::fs::read_to_string(&launch.files[0]).unwrap();
        let json: serde_json::Value = serde_json::from_str(&cfg).expect("valid json");
        let server = &json["mcpServers"]["panopt"];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["command"], "/abs/panopt");
        let args: Vec<String> = server["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(args.join(" "), "--port 7600 _mcp-proxy --host 127.0.0.1 --ws /home/u/proj --project proj-key --id u-host --name greg-main --token secret-token");
        // No unrendered placeholders leaked through.
        assert!(!cfg.contains("{{"), "unrendered placeholder in {cfg}");
    }

    #[test]
    fn env_and_flag_identity_render_from_the_same_engine() {
        // A second profile shape: identity via env vars + flags, no files.
        // Proves the engine has no claude-specific branch.
        let toml = r#"
[codex-ish]
display_name = "Codex-ish"
default_args = ["--project", "{{project}}"]
[codex-ish.spawn]
argv = ["codex", "--server", "http://{{host}}:{{port}}", "--token", "{{token}}"]
[codex-ish.spawn.env]
CODEX_SESSION = "{{agent_id}}"
CODEX_NAME = "{{name}}"
"#;
        let set = ProfileSet::from_layers(toml, None).expect("load");
        let profile = set.get("codex-ish").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let launch = build_launch(profile, &sample_facts(), dir.path()).expect("render");

        assert_eq!(
            launch.argv,
            vec![
                "codex",
                "--server",
                "http://127.0.0.1:7600",
                "--token",
                "secret-token",
                // default_args appended after argv
                "--project",
                "proj-key",
            ]
        );
        assert_eq!(
            launch.env.get("CODEX_SESSION").map(String::as_str),
            Some("u-host")
        );
        assert_eq!(
            launch.env.get("CODEX_NAME").map(String::as_str),
            Some("greg-main")
        );
        assert!(launch.files.is_empty());
    }

    #[test]
    fn model_renders_empty_when_unset_and_value_when_set() {
        let toml = r#"
[m]
display_name = "M"
[m.spawn]
argv = ["m", "--model", "{{model}}"]
"#;
        let set = ProfileSet::from_layers(toml, None).unwrap();
        let profile = set.get("m").unwrap();
        let dir = tempfile::tempdir().unwrap();

        let mut facts = sample_facts();
        facts.model = None;
        let launch = build_launch(profile, &facts, dir.path()).unwrap();
        assert_eq!(launch.argv, vec!["m", "--model", ""]);

        facts.model = Some("opus".into());
        let launch = build_launch(profile, &facts, dir.path()).unwrap();
        assert_eq!(launch.argv, vec!["m", "--model", "opus"]);
    }

    #[test]
    fn sanitize_blocks_path_traversal_in_file_names() {
        assert_eq!(sanitize("mcp_config"), "mcp_config");
        assert_eq!(sanitize("../escape"), "___escape");
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize(".."), "__");
        assert_eq!(sanitize("ok.json"), "ok_json");
    }

    #[test]
    fn unterminated_placeholder_errors() {
        let err = substitute("a {{oops", &|_| Ok(String::new())).unwrap_err();
        assert!(matches!(err, RenderError::Unterminated));
    }

    #[test]
    fn instructions_render_facts_including_process_id() {
        let toml = r#"
[orchestrated]
display_name = "Orchestrated"
instructions = "You are #{{process_id}} ({{name}}). Token {{token}} on {{host}}:{{port}}."
[orchestrated.spawn]
argv = ["agent"]
"#;
        let set = ProfileSet::from_layers(toml, None).expect("load");
        let profile = set.get("orchestrated").unwrap();
        let mut facts = sample_facts();
        facts.process_id = Some(42);
        let rendered = render_instructions(profile, &facts)
            .expect("render")
            .expect("instructions present");
        assert_eq!(
            rendered,
            "You are #42 (greg-main). Token secret-token on 127.0.0.1:7600."
        );
    }

    #[test]
    fn instructions_absent_render_to_none() {
        let toml = r#"
[plain]
display_name = "Plain"
[plain.spawn]
argv = ["agent"]
"#;
        let set = ProfileSet::from_layers(toml, None).expect("load");
        let profile = set.get("plain").unwrap();
        assert!(render_instructions(profile, &sample_facts())
            .expect("render")
            .is_none());
    }
}
