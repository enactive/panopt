//! [`Locks`] - the in-memory table of advisory locks, grouped by project.
//!
//! A lock is a named claim held by one agent. Like the agent registry, locks
//! are ephemeral and never persisted: a lock is meaningful only relative to a
//! live holder, so a daemon restart - which disconnects every agent - correctly
//! clears them. A lock is also released automatically when its holder is pruned
//! from the registry; [`crate::Store`] drives that cascade.

use std::collections::HashMap;
use std::time::SystemTime;

use crate::model::Lock;

/// Per-project lock tables. The outer key is the raw project id; the inner key
/// is the lock name.
#[derive(Default)]
pub(crate) struct Locks {
    by_project: HashMap<i64, HashMap<String, Lock>>,
}

impl Locks {
    /// Acquire `name` in `project` for agent `key`.
    ///
    /// Returns `None` if the caller now holds the lock - whether it acquired it
    /// fresh or already held it (in which case `note`, when given, is updated).
    /// Returns `Some(holder_key)` if another agent holds it, leaving it
    /// untouched. Never blocks.
    pub(crate) fn acquire(
        &mut self,
        project: i64,
        key: &str,
        name: String,
        note: Option<String>,
    ) -> Option<String> {
        let locks = self.by_project.entry(project).or_default();
        match locks.get_mut(&name) {
            Some(existing) if existing.holder_key != key => Some(existing.holder_key.clone()),
            Some(existing) => {
                // Re-acquire by the same holder: refresh the note if a new one
                // was given, but keep the original acquired_at.
                if let Some(note) = note {
                    existing.note = note;
                }
                None
            }
            None => {
                locks.insert(
                    name.clone(),
                    Lock {
                        name,
                        holder_key: key.to_string(),
                        holder_name: String::new(),
                        note: note.unwrap_or_default(),
                        acquired_at: SystemTime::now(),
                    },
                );
                None
            }
        }
    }

    /// Release `name` in `project` on behalf of agent `key`.
    ///
    /// Returns `None` if the lock is now free - released by the caller, or
    /// never held at all. Returns `Some(holder_key)` if another agent holds it,
    /// leaving it untouched.
    pub(crate) fn release(&mut self, project: i64, key: &str, name: &str) -> Option<String> {
        // No project entry means no locks: the lock is already free.
        let locks = self.by_project.get_mut(&project)?;
        match locks.get(name) {
            Some(existing) if existing.holder_key != key => Some(existing.holder_key.clone()),
            Some(_) => {
                locks.remove(name);
                None
            }
            None => None,
        }
    }

    /// Release every lock in `project` held by agent `key`, returning how many
    /// were released. Used to drop a pruned agent's locks.
    pub(crate) fn release_all(&mut self, project: i64, key: &str) -> usize {
        let Some(locks) = self.by_project.get_mut(&project) else {
            return 0;
        };
        let before = locks.len();
        locks.retain(|_, lock| lock.holder_key != key);
        before - locks.len()
    }

    /// Every lock in `project`, ordered by acquisition time then name so the
    /// list and its projected file have a stable order. `holder_name` is left
    /// empty for the caller to resolve against the registry.
    pub(crate) fn list(&self, project: i64) -> Vec<Lock> {
        let mut locks: Vec<Lock> = self
            .by_project
            .get(&project)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        locks.sort_by(|a, b| {
            a.acquired_at
                .cmp(&b.acquired_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        locks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_grants_then_blocks_others() {
        let mut l = Locks::default();
        assert_eq!(l.acquire(1, "a", "auth".into(), None), None); // a takes it
        assert_eq!(l.acquire(1, "a", "auth".into(), None), None); // a re-acquires
        assert_eq!(
            l.acquire(1, "b", "auth".into(), None),
            Some("a".to_string())
        ); // b is denied
    }

    #[test]
    fn release_frees_only_for_the_holder() {
        let mut l = Locks::default();
        l.acquire(1, "a", "auth".into(), None);
        assert_eq!(l.release(1, "b", "auth"), Some("a".to_string())); // b cannot
        assert_eq!(l.release(1, "a", "auth"), None); // a can
        assert_eq!(l.acquire(1, "b", "auth".into(), None), None); // now free for b
    }

    #[test]
    fn release_of_a_free_lock_is_a_noop() {
        let mut l = Locks::default();
        assert_eq!(l.release(1, "a", "absent"), None);
    }

    #[test]
    fn release_all_drops_one_holders_locks() {
        let mut l = Locks::default();
        l.acquire(1, "a", "one".into(), None);
        l.acquire(1, "a", "two".into(), None);
        l.acquire(1, "b", "three".into(), None);
        assert_eq!(l.release_all(1, "a"), 2);
        assert_eq!(l.list(1).len(), 1);
        assert_eq!(l.list(1)[0].name, "three");
    }

    #[test]
    fn re_acquire_updates_the_note() {
        let mut l = Locks::default();
        l.acquire(1, "a", "auth".into(), Some("first".into()));
        l.acquire(1, "a", "auth".into(), Some("second".into()));
        assert_eq!(l.list(1)[0].note, "second");
    }

    #[test]
    fn locks_are_project_isolated() {
        let mut l = Locks::default();
        l.acquire(1, "a", "auth".into(), None);
        // The same name in another project is free.
        assert_eq!(l.acquire(2, "b", "auth".into(), None), None);
    }
}
