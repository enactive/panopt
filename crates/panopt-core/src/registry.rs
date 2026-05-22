//! [`Registry`] - the in-memory roster of connected agents, grouped by project.
//!
//! Ephemeral by design: the registry tracks *currently connected* agents, so it
//! is never persisted. A daemon restart correctly starts with an empty roster
//! that refills as agents reconnect. [`crate::Store`] owns one `Registry` and
//! is the only caller; the projection of the roster lives there too.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::model::Agent;

/// Per-project agent rosters. The outer key is the raw project id; the inner
/// key is the agent's opaque connection key.
#[derive(Default)]
pub(crate) struct Registry {
    by_project: HashMap<i64, HashMap<String, Agent>>,
}

impl Registry {
    /// Record activity from `key` in `project`, registering it on first sight.
    /// Returns `true` if this call newly registered the agent.
    pub(crate) fn touch(&mut self, project: i64, key: &str) -> bool {
        let agents = self.by_project.entry(project).or_default();
        let now = SystemTime::now();
        match agents.get_mut(key) {
            Some(agent) => {
                agent.last_seen = now;
                false
            }
            None => {
                agents.insert(
                    key.to_string(),
                    Agent {
                        key: key.to_string(),
                        name: key.to_string(),
                        status: String::new(),
                        first_seen: now,
                        last_seen: now,
                    },
                );
                true
            }
        }
    }

    /// Drop agents in `project` not seen within `max_idle`. Returns the keys
    /// of the agents removed, so the caller can release their locks. A clock
    /// that has gone backwards never prunes.
    pub(crate) fn prune(&mut self, project: i64, max_idle: Duration) -> Vec<String> {
        let Some(agents) = self.by_project.get_mut(&project) else {
            return Vec::new();
        };
        let now = SystemTime::now();
        let mut pruned = Vec::new();
        agents.retain(|key, a| {
            let keep = now
                .duration_since(a.last_seen)
                .map(|idle| idle < max_idle)
                .unwrap_or(true);
            if !keep {
                pruned.push(key.clone());
            }
            keep
        });
        pruned
    }

    /// Prune silent agents in every project at once. Returns the
    /// `(project, key)` pairs removed, so the caller can release their locks
    /// and re-project the affected projects.
    pub(crate) fn prune_all(&mut self, max_idle: Duration) -> Vec<(i64, String)> {
        let project_ids: Vec<i64> = self.by_project.keys().copied().collect();
        let mut pruned = Vec::new();
        for pid in project_ids {
            for key in self.prune(pid, max_idle) {
                pruned.push((pid, key));
            }
        }
        pruned
    }

    /// Apply an `identify` to a registered agent. A no-op if it is not
    /// registered (the caller touches the agent before identifying it, so in
    /// practice the entry always exists).
    pub(crate) fn identify(
        &mut self,
        project: i64,
        key: &str,
        name: String,
        status: Option<String>,
    ) {
        if let Some(agent) = self
            .by_project
            .get_mut(&project)
            .and_then(|m| m.get_mut(key))
        {
            agent.name = name;
            if let Some(status) = status {
                agent.status = status;
            }
        }
    }

    /// The registry entry for `key` in `project`, if registered.
    pub(crate) fn get(&self, project: i64, key: &str) -> Option<Agent> {
        self.by_project
            .get(&project)
            .and_then(|m| m.get(key))
            .cloned()
    }

    /// Every agent in `project`, ordered by first-seen then key so the list
    /// and its projected file have a stable order.
    pub(crate) fn list(&self, project: i64) -> Vec<Agent> {
        let mut agents: Vec<Agent> = self
            .by_project
            .get(&project)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        agents.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then_with(|| a.key.cmp(&b.key))
        });
        agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_registers_then_only_updates() {
        let mut r = Registry::default();
        assert!(r.touch(1, "a")); // newly registered
        assert!(!r.touch(1, "a")); // already present
        assert_eq!(r.list(1).len(), 1);
    }

    #[test]
    fn rosters_are_project_isolated() {
        let mut r = Registry::default();
        r.touch(1, "a");
        r.touch(2, "b");
        assert_eq!(r.list(1).len(), 1);
        assert_eq!(r.list(2).len(), 1);
        assert_eq!(r.list(1)[0].key, "a");
    }

    #[test]
    fn identify_sets_name_and_status() {
        let mut r = Registry::default();
        r.touch(1, "a");
        r.identify(1, "a", "claude".into(), Some("working".into()));
        let agent = r.get(1, "a").unwrap();
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.status, "working");
    }

    #[test]
    fn prune_all_sweeps_every_project() {
        let mut r = Registry::default();
        r.touch(1, "a");
        r.touch(2, "b");
        r.touch(2, "c");
        let pruned = r.prune_all(Duration::ZERO);
        assert_eq!(pruned.len(), 3);
        assert!(r.list(1).is_empty());
        assert!(r.list(2).is_empty());
    }

    #[test]
    fn prune_drops_only_idle_agents() {
        let mut r = Registry::default();
        r.touch(1, "a");
        // Just touched: a generous window keeps it.
        assert!(r.prune(1, Duration::from_secs(3600)).is_empty());
        // A zero window means nothing qualifies as recent.
        assert_eq!(r.prune(1, Duration::ZERO), vec!["a".to_string()]);
        assert!(r.list(1).is_empty());
    }
}
