//! [`Registry`] - the in-memory roster of agents the daemon knows about,
//! grouped by project.
//!
//! Ephemeral by design: the registry is never persisted. A daemon restart
//! starts with an empty roster that refills as agents reconnect.
//!
//! Two kinds of entries cohabit (see [`KeySource`]): declared identities
//! (stable `?agent=<id>` keys) persist until explicit `agent_leave` or daemon
//! restart, while session identities (rotating `mcp-session-id` headers)
//! idle-prune. Without that split a quiet stable-id agent silently disappears
//! during the next sweep.
//!
//! [`crate::Store`] owns one `Registry` and is the only caller; projection of
//! the roster lives there too.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::model::{Agent, KeySource};

/// Per-project agent rosters. The outer key is the raw project id; the inner
/// key is the agent's opaque connection key.
#[derive(Default)]
pub(crate) struct Registry {
    by_project: HashMap<i64, HashMap<String, Agent>>,
}

impl Registry {
    /// Record activity from `key` in `project`, registering it on first sight.
    /// Returns `true` if this call newly registered the agent.
    ///
    /// `source` is stamped on the entry only at first-sight; re-touches never
    /// overwrite it. A key that started as `Declared` stays declared for the
    /// life of the entry, even if a malformed later request happens to omit
    /// the `?agent=` parameter.
    pub(crate) fn touch(&mut self, project: i64, key: &str, source: KeySource) -> bool {
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
                        key_source: source,
                        first_seen: now,
                        last_seen: now,
                    },
                );
                true
            }
        }
    }

    /// Drop idle [`KeySource::Session`] agents in `project`. Returns the keys
    /// removed so the caller can release their locks.
    ///
    /// [`KeySource::Declared`] entries are retained unconditionally - their
    /// presence is a property of identity, not freshness, and a long quiet
    /// stretch is not a signal they have left. A clock that has gone
    /// backwards never prunes.
    pub(crate) fn prune(&mut self, project: i64, max_idle: Duration) -> Vec<String> {
        let Some(agents) = self.by_project.get_mut(&project) else {
            return Vec::new();
        };
        let now = SystemTime::now();
        let mut pruned = Vec::new();
        agents.retain(|key, a| {
            if a.key_source == KeySource::Declared {
                return true;
            }
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

    /// Remove `key` from `project`'s roster regardless of source. Returns the
    /// removed [`Agent`] so the caller can log it and release any locks the
    /// agent held. Used by the cooperative `agent_leave` path and the
    /// launcher's pane-death hook.
    pub(crate) fn remove(&mut self, project: i64, key: &str) -> Option<Agent> {
        self.by_project
            .get_mut(&project)
            .and_then(|m| m.remove(key))
    }

    /// Test-only: rewind an entry's `last_seen` by `by` so sweep tests can
    /// pretend time has passed without sleeping.
    #[cfg(test)]
    pub(crate) fn test_backdate_last_seen(&mut self, project: i64, key: &str, by: Duration) {
        if let Some(agent) = self
            .by_project
            .get_mut(&project)
            .and_then(|m| m.get_mut(key))
        {
            agent.last_seen = agent.last_seen.checked_sub(by).expect("clock underflow");
        }
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

    /// Number of agents touched within `within` across every project: an
    /// approximation of *live HTTP connections right now*, distinct from
    /// [`Self::total`] which also counts stale declared identities.
    pub(crate) fn active_total(&self, within: Duration) -> usize {
        let now = SystemTime::now();
        self.by_project
            .values()
            .flat_map(|m| m.values())
            .filter(|a| {
                now.duration_since(a.last_seen)
                    .map(|idle| idle <= within)
                    .unwrap_or(true)
            })
            .count()
    }

    /// Per-project breakdown matching [`Self::active_total`]. Projects with
    /// zero live agents are omitted so the SIGTERM log only mentions the
    /// projects that actually have a connected client.
    pub(crate) fn active_counts_by_project(&self, within: Duration) -> Vec<(i64, usize)> {
        let now = SystemTime::now();
        let mut counts: Vec<(i64, usize)> = self
            .by_project
            .iter()
            .filter_map(|(pid, m)| {
                let n = m
                    .values()
                    .filter(|a| {
                        now.duration_since(a.last_seen)
                            .map(|idle| idle <= within)
                            .unwrap_or(true)
                    })
                    .count();
                (n > 0).then_some((*pid, n))
            })
            .collect();
        counts.sort_by_key(|(pid, _)| *pid);
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_registers_then_only_updates() {
        let mut r = Registry::default();
        assert!(r.touch(1, "a", KeySource::Session)); // newly registered
        assert!(!r.touch(1, "a", KeySource::Session)); // already present
        assert_eq!(r.list(1).len(), 1);
    }

    #[test]
    fn touch_stamps_source_on_first_sight_only() {
        let mut r = Registry::default();
        r.touch(1, "a", KeySource::Declared);
        // A later touch under a different source must not downgrade the entry:
        // a `?agent=` URL can't randomly forget itself, so if it ever did, the
        // declared identity wins by construction.
        r.touch(1, "a", KeySource::Session);
        assert_eq!(r.get(1, "a").unwrap().key_source, KeySource::Declared);
    }

    #[test]
    fn rosters_are_project_isolated() {
        let mut r = Registry::default();
        r.touch(1, "a", KeySource::Session);
        r.touch(2, "b", KeySource::Session);
        assert_eq!(r.list(1).len(), 1);
        assert_eq!(r.list(2).len(), 1);
        assert_eq!(r.list(1)[0].key, "a");
    }

    #[test]
    fn identify_sets_name_and_status() {
        let mut r = Registry::default();
        r.touch(1, "a", KeySource::Session);
        r.identify(1, "a", "claude".into(), Some("working".into()));
        let agent = r.get(1, "a").unwrap();
        assert_eq!(agent.name, "claude");
        assert_eq!(agent.status, "working");
    }

    #[test]
    fn prune_all_sweeps_every_project() {
        let mut r = Registry::default();
        r.touch(1, "a", KeySource::Session);
        r.touch(2, "b", KeySource::Session);
        r.touch(2, "c", KeySource::Session);
        let pruned = r.prune_all(Duration::ZERO);
        assert_eq!(pruned.len(), 3);
        assert!(r.list(1).is_empty());
        assert!(r.list(2).is_empty());
    }

    #[test]
    fn prune_drops_only_idle_agents() {
        let mut r = Registry::default();
        r.touch(1, "a", KeySource::Session);
        // Just touched: a generous window keeps it.
        assert!(r.prune(1, Duration::from_secs(3600)).is_empty());
        // A zero window means nothing qualifies as recent.
        assert_eq!(r.prune(1, Duration::ZERO), vec!["a".to_string()]);
        assert!(r.list(1).is_empty());
    }

    #[test]
    fn prune_keeps_declared_agents() {
        let mut r = Registry::default();
        r.touch(1, "stable", KeySource::Declared);
        r.touch(1, "ephemeral", KeySource::Session);
        // Even with a zero idle window the declared entry survives; the
        // session entry goes.
        assert_eq!(r.prune(1, Duration::ZERO), vec!["ephemeral".to_string()]);
        let keys: Vec<String> = r.list(1).into_iter().map(|a| a.key).collect();
        assert_eq!(keys, vec!["stable".to_string()]);
    }

    #[test]
    fn remove_evicts_regardless_of_source() {
        let mut r = Registry::default();
        r.touch(1, "stable", KeySource::Declared);
        let removed = r.remove(1, "stable").expect("entry was present");
        assert_eq!(removed.key, "stable");
        assert!(r.list(1).is_empty());
        // Idempotent: removing a missing key is a no-op.
        assert!(r.remove(1, "stable").is_none());
    }
}
