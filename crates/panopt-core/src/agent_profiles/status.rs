//! The status interpreter: derive an agent's activity from its output.
//!
//! The status half of "a type is data, not code". A [`StatusMatcher`] compiles
//! a profile's regex patterns once, then [`StatusMatcher::classify`] maps a
//! snapshot of the agent's output to an [`AgentState`]. Adding a state pattern
//! is a profile edit; there is no per-type code here.
//!
//! **Output source contract.** The engine takes a `&str` - the most recent
//! window of the agent's terminal output (a TUI snapshot or a log tail). It is
//! deliberately blind to *where* those bytes come from: the lifecycle layer
//! (#142) owns capturing them from a real pane/process and how large a window
//! to keep. That keeps this engine pure and fixture-testable.
//!
//! **Precedence.** Idle is the implicit default - nothing matched means nothing
//! is happening. The non-idle states are checked in a fixed priority so an
//! ambiguous snapshot resolves deterministically: `waiting` (the agent needs a
//! human - most actionable) beats `thinking` (actively working) beats `done` (a
//! completion marker). A profile that lists `idle` patterns is harmless but
//! redundant; idle is reached by fallthrough.

use regex::Regex;

use super::{AgentState, StatusRules};

/// The order non-idle states are tested in; first match wins. Idle is the
/// fallthrough and so is not listed.
const PRECEDENCE: [AgentState; 3] = [AgentState::Waiting, AgentState::Thinking, AgentState::Done];

/// A profile's status patterns, compiled to regexes and ready to classify
/// output. Build once per agent type at startup; reuse across snapshots.
#[derive(Debug)]
pub struct StatusMatcher {
    /// `(state, regexes)` in [`PRECEDENCE`] order, skipping states the profile
    /// gives no patterns for.
    rules: Vec<(AgentState, Vec<Regex>)>,
}

impl StatusMatcher {
    /// Compile a profile's status rules. Invalid regexes fail here (at startup),
    /// not at classify time. This is also where a bad pattern that slipped past
    /// `ProfileSet` load validation is caught.
    pub fn compile(rules: &StatusRules) -> Result<Self, StatusError> {
        let mut compiled = Vec::new();
        for state in PRECEDENCE {
            let Some(patterns) = rules.0.get(&state) else {
                continue;
            };
            let regexes = patterns
                .iter()
                .map(|p| {
                    Regex::new(p).map_err(|source| StatusError::Pattern {
                        state,
                        pattern: p.clone(),
                        source,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            compiled.push((state, regexes));
        }
        Ok(StatusMatcher { rules: compiled })
    }

    /// Classify an output snapshot. Returns the highest-precedence state whose
    /// any pattern matches, or [`AgentState::Idle`] if none do.
    pub fn classify(&self, output: &str) -> AgentState {
        for (state, regexes) in &self.rules {
            if regexes.iter().any(|r| r.is_match(output)) {
                return *state;
            }
        }
        AgentState::Idle
    }
}

/// Errors from compiling a profile's status patterns.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    /// A status pattern is not a valid regex.
    #[error("status pattern for {state:?} is not a valid regex ({pattern:?}): {source}")]
    Pattern {
        state: AgentState,
        pattern: String,
        #[source]
        source: regex::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_profiles::ProfileSet;

    fn claude_matcher() -> StatusMatcher {
        let set = ProfileSet::load().expect("load");
        StatusMatcher::compile(&set.get("claude-code").unwrap().status).expect("compile")
    }

    #[test]
    fn claude_fixtures_classify() {
        let m = claude_matcher();
        // A working spinner line carries the interrupt hint.
        assert_eq!(
            m.classify("✻ Compacting… (12s · esc to interrupt)"),
            AgentState::Thinking
        );
        // An approval prompt is waiting on the human.
        assert_eq!(
            m.classify("Do you want to proceed?\n  1. Yes\n  2. No"),
            AgentState::Waiting
        );
        // A plain ready prompt with no markers is idle.
        assert_eq!(m.classify("\u{203a} "), AgentState::Idle);
        assert_eq!(m.classify(""), AgentState::Idle);
    }

    #[test]
    fn precedence_waiting_beats_thinking() {
        // A snapshot that contains both markers resolves to the higher-priority
        // state deterministically.
        let toml = r#"
[a]
display_name = "A"
[a.spawn]
argv = ["a"]
[a.status]
thinking = ['esc to interrupt']
waiting = ['Do you want']
"#;
        let set = ProfileSet::from_layers(toml, None).unwrap();
        let m = StatusMatcher::compile(&set.get("a").unwrap().status).unwrap();
        assert_eq!(
            m.classify("Do you want to retry? (esc to interrupt)"),
            AgentState::Waiting
        );
    }

    #[test]
    fn new_pattern_is_a_profile_edit_not_code() {
        // Adding a `done` pattern via TOML alone changes classification.
        let toml = r#"
[a]
display_name = "A"
[a.spawn]
argv = ["a"]
[a.status]
done = ['✓ Completed']
"#;
        let set = ProfileSet::from_layers(toml, None).unwrap();
        let m = StatusMatcher::compile(&set.get("a").unwrap().status).unwrap();
        assert_eq!(m.classify("✓ Completed in 3.2s"), AgentState::Done);
        assert_eq!(m.classify("still going"), AgentState::Idle);
    }

    #[test]
    fn invalid_regex_fails_compile() {
        let toml = r#"
[a]
display_name = "A"
[a.spawn]
argv = ["a"]
[a.status]
thinking = ['(unclosed']
"#;
        let set = ProfileSet::from_layers(toml, None).unwrap();
        let err = StatusMatcher::compile(&set.get("a").unwrap().status).unwrap_err();
        assert!(matches!(err, StatusError::Pattern { .. }));
    }

    #[test]
    fn empty_status_always_idle() {
        let m = StatusMatcher::compile(&StatusRules::default()).unwrap();
        assert_eq!(m.classify("anything at all"), AgentState::Idle);
    }
}
