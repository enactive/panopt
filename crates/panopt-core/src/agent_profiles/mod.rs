//! Agent-type profiles - the top of PANopt's three-level agent model.
//!
//! A *type* (claude-code, codex, ...) is pure data: a [`AgentProfile`] that
//! parameterizes generic interpreters rather than carrying per-type code. The
//! [`AgentProfile::spawn`] half is a template the spawn interpreter (a later
//! step) renders into a concrete `(argv, env, files)` launch; the
//! [`AgentProfile::status`] half is regexes the status interpreter matches
//! against an agent's output. New types that fit this shape are new rows in the
//! TOML, not new code.
//!
//! Profiles are *not* project resources: they are global, referenced by a
//! `tool_type` string (not a `#N` id), and live in files rather than SQLite -
//! keeping them out of the unified per-project id namespace. Two layers merge,
//! both global:
//!
//! 1. **Shipped defaults** - `defaults.toml`, compiled in via [`include_str!`].
//!    claude-code lives here. Travels with the binary; the DB cannot ship
//!    default rows without a seed migration.
//! 2. **User override** - `<config-dir>/panopt/agent-types.toml`, absent by
//!    default. `config_dir()` holds user config (matching the cockpit's
//!    `viewstate`), as opposed to `data_dir()` which holds runtime state.
//!
//! The merge is keyed by `tool_type`: an override can add a new type or amend a
//! shipped one. Within a profile, **scalars override, maps (`env`, `files`,
//! status `patterns`) merge by inner key, and lists (`argv`, each pattern list)
//! replace wholesale** - so a user can repoint a binary (scalar), add one env
//! var (map-merge), or swap a command line (list-replace) without restating the
//! rest. This falls out of a generic deep-merge of the TOML tables: tables
//! recurse, everything else replaces.
//!
//! Placeholders are validated at load (fail-fast): every `{{name}}` in a spawn
//! template must be a [`KNOWN_PLACEHOLDERS`] entry or a `{{file:NAME}}` whose
//! `NAME` is a key in that profile's `files`. A typo fails the daemon at
//! startup rather than at spawn time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

mod render;
mod status;
pub use render::{build_launch, Facts, Launch, RenderError};
pub use status::{StatusError, StatusMatcher};

/// The shipped default profile set, compiled into the binary.
const DEFAULTS_TOML: &str = include_str!("defaults.toml");

/// The facts the spawn interpreter can substitute into a spawn template. A
/// `{{placeholder}}` naming anything outside this set (other than
/// `{{file:NAME}}`) is rejected at load. The spawn interpreter (#137) is the
/// other half of this contract: it must *provide* a value for each name here.
///
/// - `panopt_bin` - absolute path of the running launcher, so the spawned proxy
///   is the same panopt binary regardless of the agent's `PATH`.
/// - `host` / `port` - where panoptd listens.
/// - `ws` - the project's workspace path (absolute).
/// - `project` - the project identity key.
/// - `agent_id` / `name` - the agent's stable id and friendly display name.
/// - `token` - the daemon's bearer token.
/// - `model` - the configured model, when a config pins one.
pub const KNOWN_PLACEHOLDERS: &[&str] = &[
    "panopt_bin",
    "host",
    "port",
    "ws",
    "project",
    "agent_id",
    "name",
    "token",
    "model",
];

/// The activity states the status interpreter derives from agent output. Used
/// here only as the key type for a profile's status patterns; the matching
/// semantics (precedence, idle fallthrough) belong to the interpreter (#138).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Thinking,
    Idle,
    Waiting,
    Done,
}

/// The launch template for an agent type. Rendered by the spawn interpreter
/// into a concrete process launch; this struct only holds the un-rendered
/// templates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnSpec {
    /// The argument vector, each element a template. `argv[0]` is the program.
    pub argv: Vec<String>,
    /// Environment variables to set on the child, values templated. Keys are
    /// literal.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Files to materialize before spawn: name -> templated contents. Each is
    /// written to a temp file whose path replaces `{{file:NAME}}` in `argv`
    /// and `env`.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// The status rules for an agent type: each activity state maps to an ordered
/// list of regex patterns matched against the agent's output. A
/// [`std::collections::BTreeMap`] newtype so the public type is stable while
/// the interpreter (#138) grows methods on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct StatusRules(pub BTreeMap<AgentState, Vec<String>>);

/// One agent type. The top-level value in `defaults.toml` and the override,
/// keyed by its `tool_type` string.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    /// Human-facing label for spawn UIs.
    pub display_name: String,
    /// The model a config spawned from this type defaults to, when the agent
    /// takes one. `None` leaves it to the agent's own default.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Extra arguments appended after `spawn.argv` by default. Empty unless the
    /// type wants standing flags.
    #[serde(default)]
    pub default_args: Vec<String>,
    /// How to launch an instance of this type.
    pub spawn: SpawnSpec,
    /// How to read an instance's activity from its output.
    #[serde(default)]
    pub status: StatusRules,
}

/// The merged, validated set of agent profiles, keyed by `tool_type`.
#[derive(Debug, Clone)]
pub struct ProfileSet {
    profiles: BTreeMap<String, AgentProfile>,
}

impl ProfileSet {
    /// Load the shipped defaults, merge the user override at
    /// `<config-dir>/panopt/agent-types.toml` if it exists, and validate. This
    /// is the production entry point; tests use [`ProfileSet::from_layers`] to
    /// supply layers directly.
    pub fn load() -> Result<Self, ProfileError> {
        let override_toml = match Self::override_path() {
            Some(path) if path.exists() => {
                Some(std::fs::read_to_string(&path).map_err(ProfileError::Io)?)
            }
            _ => None,
        };
        Self::from_layers(DEFAULTS_TOML, override_toml.as_deref())
    }

    /// Merge an explicit default layer with an optional override layer and
    /// validate the result. Pure: no filesystem access, so the merge and
    /// validation rules are unit-testable without seeding a config dir.
    pub fn from_layers(
        default_toml: &str,
        override_toml: Option<&str>,
    ) -> Result<Self, ProfileError> {
        let mut table: toml::Table =
            toml::from_str(default_toml).map_err(|source| ProfileError::Parse {
                layer: "default",
                source,
            })?;
        if let Some(over) = override_toml {
            let over: toml::Table = toml::from_str(over).map_err(|source| ProfileError::Parse {
                layer: "override",
                source,
            })?;
            merge_tables(&mut table, over);
        }
        let profiles: BTreeMap<String, AgentProfile> = toml::Value::Table(table)
            .try_into()
            .map_err(|source| ProfileError::Parse {
                layer: "merged",
                source,
            })?;
        let set = ProfileSet { profiles };
        set.validate()?;
        Ok(set)
    }

    /// The user override path, `<config-dir>/panopt/agent-types.toml`. `None`
    /// only when no per-user config directory exists at all.
    fn override_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("panopt").join("agent-types.toml"))
    }

    /// The profile for a `tool_type`, or `None` if no type carries that key.
    pub fn get(&self, tool_type: &str) -> Option<&AgentProfile> {
        self.profiles.get(tool_type)
    }

    /// Whether a `tool_type` is a known profile. The check #139 uses to
    /// validate an `agent_tools.tool_type` before accepting it.
    pub fn contains(&self, tool_type: &str) -> bool {
        self.profiles.contains_key(tool_type)
    }

    /// The known `tool_type` keys, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    /// `(tool_type, profile)` pairs, sorted by key.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AgentProfile)> {
        self.profiles.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of known types.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Whether no types are defined. Always false for a normally-loaded set
    /// (the shipped defaults are never empty), but kept for completeness.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Reject any spawn template that references an unknown placeholder. Run
    /// once at load so a typo fails fast rather than at spawn time.
    fn validate(&self) -> Result<(), ProfileError> {
        for (tool_type, profile) in &self.profiles {
            let files: BTreeSet<&str> = profile.spawn.files.keys().map(String::as_str).collect();
            let check = |template: &str| -> Result<(), ProfileError> {
                for placeholder in placeholders(template) {
                    let known = match placeholder.strip_prefix("file:") {
                        Some(name) => files.contains(name),
                        None => KNOWN_PLACEHOLDERS.contains(&placeholder),
                    };
                    if !known {
                        return Err(ProfileError::UnknownPlaceholder {
                            tool_type: tool_type.clone(),
                            placeholder: placeholder.to_string(),
                        });
                    }
                }
                Ok(())
            };
            for arg in &profile.spawn.argv {
                check(arg)?;
            }
            for value in profile.spawn.env.values() {
                check(value)?;
            }
            for contents in profile.spawn.files.values() {
                check(contents)?;
            }
        }
        Ok(())
    }
}

/// Recursively merge `over` into `base`. Tables merge key-by-key; every other
/// value (scalar or array) in `over` replaces the one in `base`. This is the
/// whole merge policy: maps merge, lists and scalars replace.
fn merge_tables(base: &mut toml::Table, over: toml::Table) {
    for (key, over_value) in over {
        match (base.get_mut(&key), over_value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(over_table)) => {
                merge_tables(base_table, over_table);
            }
            (_, over_value) => {
                base.insert(key, over_value);
            }
        }
    }
}

/// The `{{...}}` placeholder names in a template, trimmed, in order. An
/// unterminated `{{` ends the scan. Slices borrow from `template`.
fn placeholders(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            break;
        };
        out.push(after[..close].trim());
        rest = &after[close + 2..];
    }
    out
}

/// Errors from loading the agent-type profiles.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// A TOML layer failed to parse or deserialize into the profile schema.
    #[error("parsing agent profiles ({layer} layer): {source}")]
    Parse {
        layer: &'static str,
        #[source]
        source: toml::de::Error,
    },

    /// The user override file exists but could not be read.
    #[error("reading agent profile override: {0}")]
    Io(#[source] std::io::Error),

    /// A spawn template names a placeholder that is neither a known fact nor a
    /// `file:NAME` declared in the profile's `files`.
    #[error("agent profile '{tool_type}' references unknown placeholder '{placeholder}'")]
    UnknownPlaceholder {
        tool_type: String,
        placeholder: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_load_and_validate() {
        let set = ProfileSet::from_layers(DEFAULTS_TOML, None).expect("defaults must load");
        let claude = set.get("claude-code").expect("claude-code present");
        assert_eq!(claude.display_name, "Claude Code");
        assert_eq!(claude.spawn.argv[0], "claude");
        assert!(claude.spawn.files.contains_key("mcp_config"));
        assert!(set.contains("claude-code"));
        assert_eq!(set.keys().collect::<Vec<_>>(), vec!["claude-code"]);
    }

    #[test]
    fn the_real_load_path_succeeds() {
        // Exercises include_str! + (absent or present) override + validation.
        // The shipped defaults must always produce a valid set.
        let set = ProfileSet::load().expect("ProfileSet::load");
        assert!(set.contains("claude-code"));
    }

    #[test]
    fn override_scalar_overrides_map_merges_list_replaces() {
        let default = r#"
[claude-code]
display_name = "Claude Code"
[claude-code.spawn]
argv = ["claude", "--mcp-config", "{{file:mcp_config}}"]
[claude-code.spawn.env]
EXISTING = "{{token}}"
[claude-code.spawn.files]
mcp_config = "{{host}}"
"#;
        let over = r#"
[claude-code]
display_name = "Claude (custom)"
[claude-code.spawn]
argv = ["claude", "--dangerously-skip-permissions"]
[claude-code.spawn.env]
ADDED = "{{name}}"
"#;
        let set = ProfileSet::from_layers(default, Some(over)).expect("merge");
        let p = set.get("claude-code").unwrap();
        // scalar: replaced
        assert_eq!(p.display_name, "Claude (custom)");
        // list: replaced wholesale (the mcp-config args are gone)
        assert_eq!(
            p.spawn.argv,
            vec!["claude", "--dangerously-skip-permissions"]
        );
        // map: merged by inner key (both env vars survive)
        assert_eq!(
            p.spawn.env.get("EXISTING").map(String::as_str),
            Some("{{token}}")
        );
        assert_eq!(
            p.spawn.env.get("ADDED").map(String::as_str),
            Some("{{name}}")
        );
        // untouched nested map survives
        assert!(p.spawn.files.contains_key("mcp_config"));
    }

    #[test]
    fn override_can_add_a_new_type() {
        let over = r#"
[my-agent]
display_name = "Homegrown"
[my-agent.spawn]
argv = ["my-agent", "--ws", "{{ws}}"]
"#;
        let set = ProfileSet::from_layers(DEFAULTS_TOML, Some(over)).expect("merge");
        assert!(set.contains("claude-code"), "shipped type still present");
        assert_eq!(set.get("my-agent").unwrap().display_name, "Homegrown");
    }

    #[test]
    fn unknown_placeholder_is_rejected_at_load() {
        let default = r#"
[bad]
display_name = "Bad"
[bad.spawn]
argv = ["x", "{{nonsense}}"]
"#;
        let err = ProfileSet::from_layers(default, None).unwrap_err();
        match err {
            ProfileError::UnknownPlaceholder {
                tool_type,
                placeholder,
            } => {
                assert_eq!(tool_type, "bad");
                assert_eq!(placeholder, "nonsense");
            }
            other => panic!("expected UnknownPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn file_placeholder_must_name_a_declared_file() {
        let default = r#"
[bad]
display_name = "Bad"
[bad.spawn]
argv = ["x", "{{file:missing}}"]
[bad.spawn.files]
present = "ok"
"#;
        let err = ProfileSet::from_layers(default, None).unwrap_err();
        assert!(matches!(err, ProfileError::UnknownPlaceholder { .. }));
    }

    #[test]
    fn known_file_placeholder_passes() {
        let default = r#"
[good]
display_name = "Good"
[good.spawn]
argv = ["x", "{{file:cfg}}"]
[good.spawn.files]
cfg = "{{host}}:{{port}}"
"#;
        ProfileSet::from_layers(default, None).expect("declared file placeholder is valid");
    }

    #[test]
    fn status_states_deserialize_and_unknown_state_errors() {
        let good = r#"
[a]
display_name = "A"
[a.spawn]
argv = ["a"]
[a.status]
thinking = ['esc to interrupt']
waiting = ['Do you want']
"#;
        let set = ProfileSet::from_layers(good, None).expect("good status");
        let status = &set.get("a").unwrap().status.0;
        assert_eq!(
            status.get(&AgentState::Thinking).unwrap(),
            &vec!["esc to interrupt"]
        );

        let bad = r#"
[a]
display_name = "A"
[a.spawn]
argv = ["a"]
[a.status]
napping = ['zzz']
"#;
        assert!(
            ProfileSet::from_layers(bad, None).is_err(),
            "unknown state rejected"
        );
    }

    #[test]
    fn placeholder_scanner_handles_edges() {
        assert_eq!(placeholders("{{a}}{{ b }}x{{c}}"), vec!["a", "b", "c"]);
        assert_eq!(placeholders("no placeholders"), Vec::<&str>::new());
        assert_eq!(placeholders("unterminated {{oops"), Vec::<&str>::new());
        assert_eq!(placeholders("{{file:cfg}}"), vec!["file:cfg"]);
    }
}
