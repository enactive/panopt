//! [`Store`] - PANopt's coordination state: persistent todos and notes,
//! plus the in-memory registry of connected agents and their advisory locks.
//!
//! Todos and notes live in a single SQLite database, scoped by a
//! `project_id`. The agent registry and the lock table are in-memory only -
//! they track *currently connected* agents and the locks they hold, which a
//! daemon restart correctly forgets. Every mutating method commits its database
//! transaction (where it has one) and then re-projects the affected `.panopt/`
//! file, so the state and the projected files can never drift: there is no code
//! path that mutates without projecting.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use crate::agent_profiles::{render_instructions, Facts, ProfileSet, DEFAULT_PROFILE_KEY};
use crate::db;
use crate::error::CoreError;
use crate::locks::Locks;
use crate::model::{
    process_status, Agent, AgentTool, AgentToolPatch, KeySource, Lock, Note, NotePatch,
    PendingInput, Priority, Process, ProcessKind, ProcessPatch, ProjectId, ProjectSummary, Todo,
    TodoComment, TodoPatch, TodoStatus,
};
use crate::projection;
use crate::registry::Registry;

/// How long a [`KeySource::Session`] agent may go silent before the registry
/// treats it as gone and prunes it (releasing its locks).
///
/// Only applies to session-keyed agents - [`KeySource::Declared`] entries
/// survive any idle stretch and only leave via `agent_leave` or daemon
/// restart, because a stable id names a *process*, not a connection.
///
/// Tool calls are the only heartbeat the daemon sees, so 30 minutes covers
/// the silent gaps in a normal conversation (typing, generation, the user
/// looking away) without letting orphaned session keys accumulate forever.
const AGENT_MAX_IDLE: Duration = Duration::from_secs(1800);

/// How recently an agent must have made a tool call to count as having a
/// *live* MCP connection for the SIGTERM gate. Short on purpose: MCP tool
/// calls are the only heartbeat the daemon sees, and an HTTP session that
/// has been silent for several seconds is - from the transport's point of
/// view - already gone, even if the registry entry persists (a declared
/// identity that has gone quiet is still in the roster but should not
/// block a daemon shutdown).
const ACTIVE_PRESENCE: Duration = Duration::from_secs(5);

/// The basename of a project path, reduced to the character set the cockpit's
/// Zellij session name accepts (ASCII alphanumerics, everything else folded to
/// `-`). Kept byte-for-byte in step with `panopt`'s `session_name` so a
/// [`ProjectSummary::name`] lets a caller rebuild the session name as
/// `panopt-<name>-<hash of root>` without re-deriving this rule. A path with no
/// final component (e.g. `/`) falls back to `project`, matching the launcher.
fn sanitized_basename(root: &str) -> String {
    let base = std::path::Path::new(root)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    base.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// All of PANopt's coordination state.
///
/// The daemon wraps one `Store` in a `Mutex`, so a `&mut self` method is the
/// natural unit of serialization: a database transaction and the file
/// re-projection that follows it complete with no other writer interleaving.
pub struct Store {
    conn: Connection,
    /// The agent-type profile registry, loaded once at [`Store::open`]. Used to
    /// validate an `agent_tools.tool_type` against the known profiles - an
    /// application-level check standing in for the SQL foreign key the
    /// file-backed registry cannot provide.
    profiles: ProfileSet,
    /// In-memory roster of connected agents. Not persisted - see [`Registry`].
    registry: Registry,
    /// In-memory advisory locks. Not persisted - see [`Locks`].
    locks: Locks,
    /// Projects whose `.panopt/` files have already been re-projected once in
    /// this process. The first touch of a project re-projects every file from
    /// the database - initializing a new project and self-healing a restarted
    /// one whose last projection may have been lost to a crash. Later touches
    /// skip that and re-project only what they change.
    reprojected: HashSet<i64>,
}

impl Store {
    /// Open (creating if absent) the SQLite database at `db_path` and migrate
    /// it to the current schema.
    pub fn open(db_path: &Path) -> Result<Self, CoreError> {
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        db::migrate(&conn)?;
        // Fail fast at startup if a shipped or override profile is malformed,
        // rather than at the first agent_tool write or spawn.
        let profiles = ProfileSet::load()?;
        Ok(Self {
            conn,
            profiles,
            registry: Registry::default(),
            locks: Locks::default(),
            reprojected: HashSet::new(),
        })
    }

    /// Resolve the project rooted at `root`, creating its row on first sight.
    ///
    /// `root` must exist; it is canonicalized, so symlinks and trailing
    /// slashes collapse onto one project. The first call for a project in this
    /// process bootstraps its `.panopt/` tree and re-projects every file from
    /// current state.
    ///
    /// This is the path-keyed entry point: identity defaults to the canonical
    /// path, exactly the pre-V10 behavior (one project per path). A caller that
    /// has resolved a stable repo key at the edge passes it explicitly via
    /// [`Store::ensure_project_by_identity`] instead.
    pub fn ensure_project(&mut self, root: &Path) -> Result<ProjectId, CoreError> {
        let canonical =
            std::fs::canonicalize(root).map_err(|_| CoreError::Workspace(root.to_path_buf()))?;
        let root_str = canonical.to_string_lossy().into_owned();
        self.ensure_project_keyed(&root_str, &canonical, &root_str)
    }

    /// Resolve a project by its opaque `identity` key, projecting into `root`.
    ///
    /// `identity` is a stable repo key resolved at the edge (the launcher /
    /// agent-config) - to core it is just an opaque string, so no git dependency
    /// crosses the core boundary. `root` is the projection location for *this*
    /// checkout (where its `.panopt/*.md` mirror is written); it is canonicalized
    /// like [`Store::ensure_project`]. Two checkouts that resolve to the same
    /// identity share one project row even when their paths differ.
    pub fn ensure_project_by_identity(
        &mut self,
        identity: &str,
        root: &Path,
    ) -> Result<ProjectId, CoreError> {
        let canonical =
            std::fs::canonicalize(root).map_err(|_| CoreError::Workspace(root.to_path_buf()))?;
        let root_str = canonical.to_string_lossy().into_owned();
        self.ensure_project_keyed(identity, &canonical, &root_str)
    }

    /// Shared core of the two `ensure_project*` entry points: look a project up
    /// by its `identity`, inserting a row (carrying both `root` and `identity`)
    /// on first sight, then bootstrap and re-project this checkout once per
    /// process. Lookup keys on `identity` rather than `root`; for path-keyed
    /// callers the two are equal, so this is behavior-identical to the pre-V10
    /// `WHERE root = ?` lookup while letting an explicit identity unify checkouts
    /// at different paths.
    ///
    /// On an identity miss it adopts a pre-existing row keyed by the same `root`:
    /// an identity-keyed caller re-keys it to `identity` (upgrading a back-filled
    /// path row, or following an identity change), while a path-keyed caller
    /// leaves it untouched. This both avoids colliding on `UNIQUE(root)` and
    /// keeps path-only clients from clobbering an established repo identity.
    fn ensure_project_keyed(
        &mut self,
        identity: &str,
        canonical: &Path,
        root_str: &str,
    ) -> Result<ProjectId, CoreError> {
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM projects WHERE identity = ?1",
                [identity],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => id,
            None => {
                // No row carries this identity yet. Before inserting, adopt a
                // legacy row keyed by this same `root` - one whose identity was
                // back-filled to its path by the V10 migration, or created by a
                // path-only client - by re-keying it to the resolved identity.
                // Without this, the first identity-aware connect for an existing
                // path-keyed project would collide on the UNIQUE(root) constraint
                // instead of upgrading the row in place.
                let legacy: Option<i64> = self
                    .conn
                    .query_row("SELECT id FROM projects WHERE root = ?1", [root_str], |r| {
                        r.get(0)
                    })
                    .optional()?;
                match legacy {
                    // A row already exists for this path under a different
                    // identity. An identity-keyed caller (the edge resolver,
                    // where `identity != root_str`) is authoritative and re-keys
                    // it - covering both the first upgrade of a back-filled path
                    // row and a later identity change (a new remote, or `project
                    // init`). A path-keyed caller (`identity == root_str`) never
                    // clobbers an identity another connection established, so it
                    // reuses the row untouched. That asymmetry stops path-only
                    // clients (the `panopt todo` CLI) from thrashing a project
                    // back to path identity between proxy connects.
                    Some(id) => {
                        if identity != root_str {
                            self.conn.execute(
                                "UPDATE projects SET identity = ?1 WHERE id = ?2",
                                rusqlite::params![identity, id],
                            )?;
                        }
                        id
                    }
                    None => {
                        self.conn.execute(
                            "INSERT INTO projects (root, identity) VALUES (?1, ?2)",
                            [root_str, identity],
                        )?;
                        self.conn.last_insert_rowid()
                    }
                }
            }
        };

        let project = ProjectId(id);
        if self.reprojected.insert(id) {
            projection::bootstrap(canonical)?;
            self.reproject_all(canonical, project)?;
        }
        Ok(project)
    }

    // --- agents ---

    /// Record activity from agent `key` in `project`, registering it on first
    /// sight, and prune any session-keyed agents that have gone silent.
    /// Re-projects whatever changed.
    ///
    /// `source` controls the entry's lifetime: [`KeySource::Declared`] keys
    /// (stable `?agent=<id>`) survive idle prunes, [`KeySource::Session`] keys
    /// (rotating `mcp-session-id` headers) age out at `AGENT_MAX_IDLE`.
    pub fn agent_touch(
        &mut self,
        project: ProjectId,
        key: &str,
        source: KeySource,
    ) -> Result<(), CoreError> {
        let added = self.registry.touch(project.0, key, source);
        let pruned_any = self.prune_agents(project)?;
        // `prune_agents` re-projects the roster when it prunes; if it did not
        // prune but this call added a new agent, the roster still changed.
        if added && !pruned_any {
            self.reproject_agents(project)?;
        }
        Ok(())
    }

    /// Test-only: rewind an agent's `last_seen` for sweep tests.
    #[cfg(test)]
    pub fn test_backdate_last_seen(&mut self, project: ProjectId, key: &str, by: Duration) {
        self.registry.test_backdate_last_seen(project.0, key, by);
    }

    /// Remove agent `key` from `project`'s roster, release every lock it
    /// holds, and re-project whatever changed. Idempotent: returns `false`
    /// when no such entry was registered.
    ///
    /// This is the cooperative counterpart to the idle sweep - it lets a
    /// declared agent leave on its own ([`KeySource::Declared`] entries are
    /// never auto-pruned), and lets the launcher tear down a cockpit-spawned
    /// agent the moment its pane closes.
    pub fn agent_leave(&mut self, project: ProjectId, key: &str) -> Result<bool, CoreError> {
        let Some(_gone) = self.registry.remove(project.0, key) else {
            return Ok(false);
        };
        let released = self.locks.release_all(project.0, key);
        self.reproject_agents(project)?;
        if released > 0 {
            self.reproject_locks(project)?;
        }
        Ok(true)
    }

    /// Set agent `key`'s name and, if given, its self-reported status.
    pub fn agent_identify(
        &mut self,
        project: ProjectId,
        key: &str,
        name: String,
        status: Option<String>,
    ) -> Result<(), CoreError> {
        self.registry.identify(project.0, key, name, status);
        self.reproject_agents(project)
    }

    /// The registry entry for agent `key`, if it is registered in `project`.
    pub fn agent_whoami(&self, project: ProjectId, key: &str) -> Option<Agent> {
        self.registry.get(project.0, key)
    }

    /// Every agent registered in `project`, after pruning silent ones.
    pub fn agent_list(&mut self, project: ProjectId) -> Result<Vec<Agent>, CoreError> {
        self.prune_agents(project)?;
        Ok(self.registry.list(project.0))
    }

    /// Number of agents with a *live* MCP connection right now, across every
    /// project. The daemon's SIGTERM guard uses this to decide whether
    /// shutting down would drop a connected client.
    ///
    /// Counts only entries touched within [`ACTIVE_PRESENCE`]: a stale
    /// declared identity (registered minutes ago, no recent tool call) does
    /// not block a shutdown - there is no live HTTP connection to drop, only
    /// a registry row, which a daemon restart correctly forgets anyway.
    /// Without this filter, the gate refuses the first SIGTERM whenever any
    /// declared agent has ever connected (todo #83), so `just refresh`
    /// always takes the full two-strike path and the down window blows past
    /// Claude Code's reconnect budget. Does not prune - this runs from the
    /// signal handler and stale counts are preferable to mutation there.
    pub fn connected_agent_count(&self) -> usize {
        self.registry.active_total(ACTIVE_PRESENCE)
    }

    /// `(project_id, agent_count)` for every project with at least one
    /// agent connected right now, using the same liveness threshold as
    /// [`Self::connected_agent_count`]. The daemon logs this on SIGTERM so
    /// the operator can see what a second SIGTERM would drop.
    pub fn connected_agents_by_project(&self) -> Vec<(ProjectId, usize)> {
        self.registry
            .active_counts_by_project(ACTIVE_PRESENCE)
            .into_iter()
            .map(|(pid, n)| (ProjectId(pid), n))
            .collect()
    }

    /// One [`ProjectSummary`] per project the daemon knows about, for the
    /// cross-project switcher board (todo #120, design note #119).
    ///
    /// This is a pure read - a join of state the daemon already holds, never
    /// new bookkeeping. The badges come from three sources: live agent counts
    /// from the in-memory registry (the same [`ACTIVE_PRESENCE`] liveness
    /// window as [`Self::connected_agents_by_project`]), advisory lock counts
    /// from the in-memory lock table, and todo status counts plus the latest
    /// mutation timestamp from SQLite. The aggregate queries are grouped by
    /// `project_id` so the whole board is three statements, not one pair per
    /// project.
    ///
    /// Rows are keyed and ordered by the project's row id (insertion order);
    /// `identity` carries the stable repo key the row is logically keyed on.
    /// Soft-deleted todos and notes are excluded, matching every other read.
    pub fn project_list(&self) -> Result<Vec<ProjectSummary>, CoreError> {
        // Live agents and held locks live in memory, not SQLite. Index agent
        // counts by raw project id once so the per-project assembly is O(1).
        let agents: HashMap<i64, usize> = self
            .registry
            .active_counts_by_project(ACTIVE_PRESENCE)
            .into_iter()
            .collect();

        // Open / in-progress todo counts in one grouped pass. Other statuses
        // (backlog, draft, terminal) are not board badges, so the query never
        // fetches them.
        let mut todo_counts: HashMap<i64, (usize, usize)> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT project_id, status, COUNT(*) FROM todos
                  WHERE deleted_at IS NULL AND status IN ('open', 'in_progress')
                  GROUP BY project_id, status",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as usize,
                ))
            })?;
            for row in rows {
                let (pid, status, count) = row?;
                let entry = todo_counts.entry(pid).or_default();
                match status.as_str() {
                    "open" => entry.0 = count,
                    "in_progress" => entry.1 = count,
                    _ => {}
                }
            }
        }

        // Latest mutation across todos and notes, per project. An empty
        // `updated_at` (legacy rows predating the column) sorts below any real
        // timestamp, so MAX surfaces a real one when present; a project whose
        // every row is empty (or that has no rows) yields no entry and reads
        // as no activity.
        let mut last_activity: HashMap<i64, String> = HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT project_id, MAX(updated_at) FROM (
                     SELECT project_id, updated_at FROM todos WHERE deleted_at IS NULL
                     UNION ALL
                     SELECT project_id, updated_at FROM notes WHERE deleted_at IS NULL
                 ) GROUP BY project_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (pid, ts) = row?;
                if let Some(ts) = ts.filter(|s| !s.is_empty()) {
                    last_activity.insert(pid, ts);
                }
            }
        }

        let mut stmt = self
            .conn
            .prepare("SELECT id, root, identity FROM projects ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;

        let mut summaries = Vec::new();
        for row in rows {
            let (pid, root, identity) = row?;
            let (todos_open, todos_in_progress) = todo_counts.get(&pid).copied().unwrap_or((0, 0));
            summaries.push(ProjectSummary {
                name: sanitized_basename(&root),
                // `identity` is back-filled to `root` for every row by the V10
                // migration, so the fallback only guards a row written between
                // the column add and its back-fill.
                identity: identity.unwrap_or_else(|| root.clone()),
                root,
                agents_active: agents.get(&pid).copied().unwrap_or(0),
                todos_open,
                todos_in_progress,
                locks_held: self.locks.list(pid).len(),
                last_activity: last_activity.remove(&pid),
            });
        }
        Ok(summaries)
    }

    /// Prune silent agents across *every* project, release their locks, and
    /// re-project what changed. Returns the keys removed.
    ///
    /// The daemon calls this on a timer so a closed agent leaves the roster
    /// even when no other agent is active to trigger a prune.
    pub fn sweep_idle_agents(&mut self) -> Result<Vec<String>, CoreError> {
        let pruned = self.registry.prune_all(AGENT_MAX_IDLE);
        if pruned.is_empty() {
            return Ok(Vec::new());
        }
        let mut affected: HashSet<i64> = HashSet::new();
        for (pid, key) in &pruned {
            self.locks.release_all(*pid, key);
            affected.insert(*pid);
        }
        for pid in affected {
            let project = ProjectId(pid);
            self.reproject_agents(project)?;
            self.reproject_locks(project)?;
        }
        Ok(pruned.into_iter().map(|(_, key)| key).collect())
    }

    /// Reap instances whose OS process is gone, flipping each from `running`
    /// to [`process_status::EXITED`] and re-projecting (todo #142, bug #163).
    ///
    /// The daemon calls this on a timer so a hand-quit agent (the user pressed
    /// `Ctrl-c`/`exit` in its pane, no `process_stop` ever ran) stops being
    /// reported as live - otherwise its `running` row lingers in `processes.md`
    /// forever and the cockpit keeps drawing a pane for a process that is gone.
    ///
    /// Liveness itself is supplied by the caller via `is_alive` (a `kill(pid, 0)`
    /// probe): the OS-signal effect belongs to the daemon, which lives on the
    /// executing host, not to the state layer - the same split as
    /// [`Self::process_stop`]. Core owns only the enumeration, the status flip,
    /// and the reprojection, so this stays unit-testable with a fake predicate.
    /// Only `running` rows carrying a pid are probed; `starting` (no pid yet)
    /// and already-terminal rows are left alone. Returns the
    /// `(project_id, process_id)` pairs reaped.
    pub fn sweep_dead_processes(
        &mut self,
        is_alive: impl Fn(i64) -> bool,
    ) -> Result<Vec<(i64, u64)>, CoreError> {
        let candidates: Vec<(i64, u64, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT project_id, id, pid FROM processes
                  WHERE deleted_at IS NULL AND status = ?1 AND pid IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![process_status::RUNNING], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let dead: Vec<(i64, u64)> = candidates
            .into_iter()
            .filter(|(_, _, pid)| !is_alive(*pid))
            .map(|(project, id, _)| (project, id))
            .collect();
        for (project, id) in &dead {
            // `process_update` re-projects the owning project, so a reaped row
            // drops out of `processes.md` immediately (terminal rows are not
            // rendered).
            self.process_update(
                ProjectId(*project),
                *id,
                ProcessPatch {
                    status: Some(Some(process_status::EXITED.to_string())),
                    ..Default::default()
                },
            )?;
        }
        Ok(dead)
    }

    /// Re-project `processes.md` for every project with a live agent instance so
    /// time-based annotations advance without a mutation to drive them - notably
    /// the `idle:` presence age (#142/#163), which is `now - last_seen` computed
    /// at render time and therefore frozen between reprojections. The daemon
    /// calls this on a ~10s timer.
    ///
    /// Cheap by construction: projects with no `starting`/`running` agent are
    /// skipped, so an idle daemon does nothing; an active one does a couple of
    /// small reads and one atomic write per such project. Returns how many
    /// projects were re-projected (for logging).
    pub fn tick_process_projections(&self) -> Result<usize, CoreError> {
        let projects: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT project_id FROM processes
                  WHERE kind = ?1 AND deleted_at IS NULL AND status IN (?2, ?3)",
            )?;
            let rows = stmt.query_map(
                params![
                    ProcessKind::Agent.as_str(),
                    process_status::STARTING,
                    process_status::RUNNING,
                ],
                |r| r.get::<_, i64>(0),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for pid in &projects {
            self.reproject_processes(ProjectId(*pid))?;
        }
        Ok(projects.len())
    }

    /// Prune agents in `project` that have gone silent, release any locks they
    /// held, and re-project whatever changed. Returns whether anything was
    /// pruned.
    fn prune_agents(&mut self, project: ProjectId) -> Result<bool, CoreError> {
        let pruned = self.registry.prune(project.0, AGENT_MAX_IDLE);
        if pruned.is_empty() {
            return Ok(false);
        }
        let mut released = 0;
        for gone in &pruned {
            released += self.locks.release_all(project.0, gone);
        }
        self.reproject_agents(project)?;
        if released > 0 {
            self.reproject_locks(project)?;
        }
        Ok(true)
    }

    // --- locks ---

    /// Acquire the advisory lock `name` in `project` for agent `key`.
    ///
    /// Non-blocking. Returns `None` if the caller now holds the lock, or
    /// `Some(holder_name)` if another agent holds it. Re-acquiring a lock you
    /// already hold succeeds and updates its note when one is given.
    pub fn lock_acquire(
        &mut self,
        project: ProjectId,
        key: &str,
        name: String,
        note: Option<String>,
    ) -> Result<Option<String>, CoreError> {
        match self.locks.acquire(project.0, key, name, note) {
            None => {
                self.reproject_locks(project)?;
                Ok(None)
            }
            Some(holder_key) => Ok(Some(self.resolve_agent_name(project, &holder_key))),
        }
    }

    /// Release the advisory lock `name` in `project` on behalf of agent `key`.
    ///
    /// Returns `None` if the lock is now free, or `Some(holder_name)` if
    /// another agent holds it (and it was left untouched).
    pub fn lock_release(
        &mut self,
        project: ProjectId,
        key: &str,
        name: &str,
    ) -> Result<Option<String>, CoreError> {
        match self.locks.release(project.0, key, name) {
            None => {
                self.reproject_locks(project)?;
                Ok(None)
            }
            Some(holder_key) => Ok(Some(self.resolve_agent_name(project, &holder_key))),
        }
    }

    /// Every advisory lock held in `project`, holder names resolved.
    pub fn lock_list(&self, project: ProjectId) -> Vec<Lock> {
        let mut locks = self.locks.list(project.0);
        for lock in &mut locks {
            lock.holder_name = self.resolve_agent_name(project, &lock.holder_key);
        }
        locks
    }

    /// The display name for an agent key: its registered name, or the key
    /// itself if it is not (or no longer) in the registry.
    fn resolve_agent_name(&self, project: ProjectId, key: &str) -> String {
        self.registry
            .get(project.0, key)
            .map(|a| a.name)
            .unwrap_or_else(|| key.to_string())
    }

    // --- notes ---

    /// Create a new, empty note in `project` and return its id.
    pub fn note_create(&mut self, project: ProjectId, title: String) -> Result<u64, CoreError> {
        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid)?;
            tx.execute(
                "INSERT INTO notes (project_id, id, title, body, tags, created_at, updated_at)
                 VALUES (?1, ?2, ?3, '', '[]', datetime('now'), datetime('now'))",
                params![pid, next, title],
            )?;
            tx.execute(
                "UPDATE projects SET next_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_note_full(project, id)?;
        Ok(id)
    }

    /// List a project's notes as `(id, title)` pairs, id-ascending.
    pub fn note_list(&self, project: ProjectId) -> Result<Vec<(u64, String)>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title FROM notes
              WHERE project_id = ?1 AND deleted_at IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Search a project's notes, returning `(id, title)` pairs in the
    /// same shape as [`Store::note_list`].
    ///
    /// `query` substring-matches `title`/`body` case-insensitively at the SQL
    /// layer; a purely numeric query (optionally `#`-prefixed) *also* matches
    /// the exact id, so a `#N` reference is reachable from search (see
    /// [`query_as_id`]). `require_tags` is applied in Rust against the JSON tag
    /// column (AND semantics), matching the parse-in-Rust pattern that
    /// [`Store::tags_list`] uses for the same column. With no filters this is
    /// equivalent to `note_list`.
    pub fn note_search(
        &self,
        project: ProjectId,
        query: Option<&str>,
        require_tags: &[String],
    ) -> Result<Vec<(u64, String)>, CoreError> {
        let mut sql = String::from(
            "SELECT id, title, tags FROM notes \
             WHERE project_id = ?1 AND deleted_at IS NULL",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project.0)];
        if let Some(q) = query {
            let pat = format!("%{}%", q.to_lowercase());
            let n = binds.len() + 1;
            binds.push(Box::new(pat));
            let mut clause = format!("LOWER(title) LIKE ?{n} OR LOWER(body) LIKE ?{n}");
            if let Some(id) = query_as_id(q) {
                let m = binds.len() + 1;
                binds.push(Box::new(id as i64));
                clause.push_str(&format!(" OR id = ?{m}"));
            }
            sql.push_str(&format!(" AND ({clause})"));
        }
        sql.push_str(" ORDER BY id");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )?;
        let rows: Vec<(u64, String, String)> = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|(_, _, tags_json)| {
                if require_tags.is_empty() {
                    return true;
                }
                let tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
                require_tags
                    .iter()
                    .all(|wanted| tags.iter().any(|got| got == wanted))
            })
            .map(|(id, title, _)| (id, title))
            .collect())
    }

    /// Append `content` to a note, separating it from existing content
    /// with a single newline.
    pub fn note_append(
        &mut self,
        project: ProjectId,
        id: u64,
        content: &str,
    ) -> Result<(), CoreError> {
        let mut body = self.note_body(project, id)?;
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(content);
        self.conn.execute(
            "UPDATE notes
                SET body = ?1, updated_at = datetime('now')
              WHERE project_id = ?2 AND id = ?3",
            params![body, project.0, id as i64],
        )?;
        self.reproject_note_full(project, id)
    }

    /// Read the full body of a note.
    pub fn note_read(&self, project: ProjectId, id: u64) -> Result<String, CoreError> {
        self.note_body(project, id)
    }

    /// Fetch one note in full - title, body, and timestamps.
    pub fn note_get(&self, project: ProjectId, id: u64) -> Result<Note, CoreError> {
        self.fetch_note(project, id)
    }

    /// Apply `patch` to note `id`. Each `None` field is left untouched.
    /// Re-projects both the per-note file and the index.
    pub fn note_update(
        &mut self,
        project: ProjectId,
        id: u64,
        patch: NotePatch,
    ) -> Result<(), CoreError> {
        let mut pad = self.fetch_note(project, id)?;
        if let Some(v) = patch.title {
            pad.title = v;
        }
        if let Some(v) = patch.body {
            pad.body = v;
        }
        if let Some(v) = patch.tags {
            pad.tags = v;
        }
        // Bad JSON would only fail here on a programmer error (Vec<String> is
        // always serializable); the fallback keeps the column valid regardless.
        let tags_json = serde_json::to_string(&pad.tags).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "UPDATE notes
                SET title = ?1, body = ?2, tags = ?3, updated_at = datetime('now')
              WHERE project_id = ?4 AND id = ?5",
            params![pad.title, pad.body, tags_json, project.0, id as i64],
        )?;
        self.reproject_note_full(project, id)
    }

    /// Soft-delete a note: stamp `deleted_at` so list / read / index
    /// paths skip it. The row stays behind so a later undelete surface has
    /// something to revive; the per-note projection file is swept by
    /// the index reprojection below (its sweep keys off "note is in the
    /// live index", which the row no longer is).
    pub fn note_delete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE notes SET deleted_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::NoteNotFound(id));
        }
        // reproject_notes_index sweeps the now-orphaned per-pad file.
        self.reproject_notes_index(project)
    }

    fn note_body(&self, project: ProjectId, id: u64) -> Result<String, CoreError> {
        self.conn
            .query_row(
                "SELECT body FROM notes
                  WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![project.0, id as i64],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(CoreError::NoteNotFound(id))
    }

    fn fetch_note(&self, project: ProjectId, id: u64) -> Result<Note, CoreError> {
        self.conn
            .query_row(
                "SELECT title, body, tags, created_at, updated_at FROM notes
                  WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![project.0, id as i64],
                |r| {
                    let tags: String = r.get(2)?;
                    Ok(Note {
                        id,
                        title: r.get(0)?,
                        body: r.get(1)?,
                        // Bad JSON yields an empty tag set so a stray manual
                        // write to the column never bricks the read path.
                        tags: serde_json::from_str(&tags).unwrap_or_default(),
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::NoteNotFound(id))
    }

    // --- agent_tools ---

    /// Whether `tool_type` names a known agent-type profile. The check surfaces
    /// (the config form's type dropdown, the spawn path) use to stay within the
    /// registry.
    pub fn known_tool_type(&self, tool_type: &str) -> bool {
        self.profiles.contains(tool_type)
    }

    /// The known agent-type keys, sorted - the choices a config form offers.
    pub fn agent_types(&self) -> Vec<String> {
        self.profiles.keys().map(str::to_owned).collect()
    }

    /// Reject a `tool_type` with no profile in the registry. The
    /// application-level stand-in for the foreign key the file-backed registry
    /// cannot provide.
    fn validate_tool_type(&self, tool_type: &str) -> Result<(), CoreError> {
        if self.profiles.contains(tool_type) {
            Ok(())
        } else {
            Err(CoreError::UnknownToolType(tool_type.to_string()))
        }
    }

    /// Map the legacy `tool_type='agent'` sentinel (and empty) onto the default
    /// profile key on read. V11 rewrites these in the database; this is the
    /// cheap safety net for a restored or un-migrated database where V11 never
    /// ran. A *specific* but unknown key is left as-is so the UI can flag it as
    /// a dangling reference rather than having it silently masked.
    fn normalize_tool_type(raw: String) -> String {
        if raw.is_empty() || raw == "agent" {
            DEFAULT_PROFILE_KEY.to_string()
        } else {
            raw
        }
    }

    /// Create an agent tool (configuration) in `project` and return its id.
    /// `position` defaults to the new id so tools sort by creation order
    /// while staying reorderable.
    #[allow(clippy::too_many_arguments)]
    pub fn agent_tool_create(
        &mut self,
        project: ProjectId,
        name: String,
        display_name: String,
        command: String,
        cwd: String,
        tool_type: String,
        system_prompt: String,
        enabled: bool,
    ) -> Result<u64, CoreError> {
        self.validate_tool_type(&tool_type)?;
        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid)?;
            tx.execute(
                "INSERT INTO agent_tools
                    (project_id, id, name, display_name, command, cwd,
                     tool_type, system_prompt, enabled, position, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, datetime('now'))",
                params![
                    pid,
                    next,
                    name,
                    display_name,
                    command,
                    cwd,
                    tool_type,
                    system_prompt,
                    enabled as i64,
                    next,
                ],
            )?;
            tx.execute(
                "UPDATE projects SET next_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_agent_tools(project)?;
        Ok(id)
    }

    /// List a project's agent tools, ordered by `position` then `id`.
    pub fn agent_tool_list(&self, project: ProjectId) -> Result<Vec<AgentTool>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, display_name, command, cwd, tool_type, system_prompt, enabled, position, created_at
               FROM agent_tools
              WHERE project_id = ?1 AND deleted_at IS NULL
              ORDER BY position, id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            Ok(AgentTool {
                id: r.get::<_, i64>(0)? as u64,
                name: r.get(1)?,
                display_name: r.get(2)?,
                command: r.get(3)?,
                cwd: r.get(4)?,
                tool_type: Self::normalize_tool_type(r.get(5)?),
                system_prompt: r.get(6)?,
                enabled: r.get::<_, i64>(7)? != 0,
                position: r.get(8)?,
                created_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch one agent tool, or [`CoreError::AgentToolNotFound`] if it is absent.
    pub fn agent_tool_get(&self, project: ProjectId, id: u64) -> Result<AgentTool, CoreError> {
        self.fetch_agent_tool(project, id)
    }

    /// Apply an [`AgentToolPatch`]: every `Some` field is written, every
    /// `None` field is left as-is.
    pub fn agent_tool_update(
        &mut self,
        project: ProjectId,
        id: u64,
        patch: AgentToolPatch,
    ) -> Result<(), CoreError> {
        let mut entry = self.fetch_agent_tool(project, id)?;
        if let Some(v) = patch.name {
            entry.name = v;
        }
        if let Some(v) = patch.display_name {
            entry.display_name = v;
        }
        if let Some(v) = patch.command {
            entry.command = v;
        }
        if let Some(v) = patch.cwd {
            entry.cwd = v;
        }
        if let Some(v) = patch.tool_type {
            self.validate_tool_type(&v)?;
            entry.tool_type = v;
        }
        if let Some(v) = patch.system_prompt {
            entry.system_prompt = v;
        }
        if let Some(v) = patch.enabled {
            entry.enabled = v;
        }
        if let Some(v) = patch.position {
            entry.position = v;
        }
        self.conn.execute(
            "UPDATE agent_tools
                SET name = ?1, display_name = ?2, command = ?3, cwd = ?4,
                    tool_type = ?5, system_prompt = ?6, enabled = ?7, position = ?8
              WHERE project_id = ?9 AND id = ?10",
            params![
                entry.name,
                entry.display_name,
                entry.command,
                entry.cwd,
                entry.tool_type,
                entry.system_prompt,
                entry.enabled as i64,
                entry.position,
                project.0,
                id as i64,
            ],
        )?;
        self.reproject_agent_tools(project)
    }

    /// Soft-delete an agent tool. Any process rows that reference it keep
    /// their `agent_tool_id` pointing at the now-soft-deleted row so a future
    /// undelete reconstitutes the link as it was; live process queries
    /// ignore the soft-deleted tool because every read path filters on
    /// `deleted_at IS NULL`.
    pub fn agent_tool_delete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE agent_tools SET deleted_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::AgentToolNotFound(id));
        }
        self.reproject_agent_tools(project)
    }

    fn fetch_agent_tool(&self, project: ProjectId, id: u64) -> Result<AgentTool, CoreError> {
        self.conn
            .query_row(
                "SELECT name, display_name, command, cwd, tool_type, system_prompt, enabled, position, created_at
                   FROM agent_tools
                  WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![project.0, id as i64],
                |r| {
                    Ok(AgentTool {
                        id,
                        name: r.get(0)?,
                        display_name: r.get(1)?,
                        command: r.get(2)?,
                        cwd: r.get(3)?,
                        tool_type: Self::normalize_tool_type(r.get(4)?),
                        system_prompt: r.get(5)?,
                        enabled: r.get::<_, i64>(6)? != 0,
                        position: r.get(7)?,
                        created_at: r.get(8)?,
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::AgentToolNotFound(id))
    }

    // --- processes ---

    /// Create a process instance in `project` and return its id.
    ///
    /// If `agent_tool_id` is `Some`, the referenced tool must exist in the
    /// same project; otherwise a [`CoreError::BadRequest`] is returned so a
    /// stale id never becomes a silent NULL.
    #[allow(clippy::too_many_arguments)]
    pub fn process_create(
        &mut self,
        project: ProjectId,
        kind: ProcessKind,
        name: String,
        display_name: String,
        command: String,
        cwd: String,
        agent_tool_id: Option<u64>,
    ) -> Result<u64, CoreError> {
        let pid = project.0;
        if let Some(tool_id) = agent_tool_id {
            // Validate at the app layer too; the FK alone would translate a
            // missing tool into an opaque SQLite constraint error.
            self.fetch_agent_tool(project, tool_id)
                .map_err(|e| match e {
                    CoreError::AgentToolNotFound(_) => CoreError::BadRequest(format!(
                        "agent_tool_id {tool_id} does not exist in this project"
                    )),
                    other => other,
                })?;
        }
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid)?;
            tx.execute(
                "INSERT INTO processes
                    (project_id, id, kind, name, display_name, command, cwd,
                     position, agent_tool_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))",
                params![
                    pid,
                    next,
                    kind.as_str(),
                    name,
                    display_name,
                    command,
                    cwd,
                    next,
                    agent_tool_id.map(|v| v as i64),
                ],
            )?;
            tx.execute(
                "UPDATE projects SET next_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_processes(project)?;
        Ok(id)
    }

    /// List a project's processes, ordered by `position` then `id`.
    pub fn process_list(&self, project: ProjectId) -> Result<Vec<Process>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, display_name, command, cwd, position,
                    agent_tool_id, pid, pane_id, status, agent_state, last_seen,
                    state_since, created_at, extra_args
               FROM processes
              WHERE project_id = ?1 AND deleted_at IS NULL
              ORDER BY position, id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            let kind: String = r.get(1)?;
            Ok(Process {
                id: r.get::<_, i64>(0)? as u64,
                kind: ProcessKind::parse(&kind).unwrap_or_default(),
                name: r.get(2)?,
                display_name: r.get(3)?,
                command: r.get(4)?,
                cwd: r.get(5)?,
                position: r.get(6)?,
                agent_tool_id: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                pid: r.get(8)?,
                pane_id: r.get(9)?,
                status: r.get(10)?,
                agent_state: r.get(11)?,
                last_seen: r.get(12)?,
                state_since: r.get(13)?,
                created_at: r.get(14)?,
                extra_args: parse_extra_args(&r.get::<_, String>(15)?),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch one process, or [`CoreError::ProcessNotFound`] if it is absent.
    pub fn process_get(&self, project: ProjectId, id: u64) -> Result<Process, CoreError> {
        self.fetch_process(project, id)
    }

    /// The cockpit's per-instance pane-output capture file. The sidebar plugin
    /// tees each running agent's viewport to `.panopt/.cockpit/output-<id>.txt`
    /// on its poll; the orchestration `process_output`/`search_output` tools read
    /// it. Cockpit-internal (under `.cockpit/`), bounded and ~1s-lagged - not a
    /// durable record, just a recent window.
    fn output_capture_path(&self, project: ProjectId, id: u64) -> Result<PathBuf, CoreError> {
        Ok(self
            .project_root(project)?
            .join(".panopt")
            .join(".cockpit")
            .join(format!("output-{id}.txt")))
    }

    /// Recent captured pane rows for instance `id` (todo #190 line): the raw
    /// orchestration channel a parent can scrape to confirm a child's progress.
    /// Returns the last `lines` rows (all when `None`), or an empty string when
    /// nothing has been captured yet. The instance must exist.
    pub fn process_output(
        &self,
        project: ProjectId,
        id: u64,
        lines: Option<u64>,
    ) -> Result<String, CoreError> {
        self.fetch_process(project, id)?;
        let body =
            std::fs::read_to_string(self.output_capture_path(project, id)?).unwrap_or_default();
        let out = match lines {
            Some(n) => {
                let rows: Vec<&str> = body.lines().collect();
                let start = rows.len().saturating_sub(n as usize);
                rows[start..].join("\n")
            }
            None => body,
        };
        Ok(out)
    }

    /// Captured pane rows of instance `id` that contain `pattern` (todo #190
    /// line): grep the raw channel for a sentinel. The instance must exist.
    pub fn search_output(
        &self,
        project: ProjectId,
        id: u64,
        pattern: &str,
    ) -> Result<Vec<String>, CoreError> {
        self.fetch_process(project, id)?;
        let body =
            std::fs::read_to_string(self.output_capture_path(project, id)?).unwrap_or_default();
        Ok(body
            .lines()
            .filter(|l| l.contains(pattern))
            .map(str::to_string)
            .collect())
    }

    /// Apply a [`ProcessPatch`]: every outer `Some` field is written. Inner
    /// nullable fields (`agent_tool_id`, `pid`, `status`, `agent_state`,
    /// `last_seen`) use `Some(Option<T>)` so a caller can both set and clear
    /// them.
    pub fn process_update(
        &mut self,
        project: ProjectId,
        id: u64,
        patch: ProcessPatch,
    ) -> Result<(), CoreError> {
        let mut entry = self.fetch_process(project, id)?;
        if let Some(v) = patch.name {
            entry.name = v;
        }
        if let Some(v) = patch.display_name {
            entry.display_name = v;
        }
        if let Some(v) = patch.command {
            entry.command = v;
        }
        if let Some(v) = patch.cwd {
            entry.cwd = v;
        }
        if let Some(v) = patch.position {
            entry.position = v;
        }
        if let Some(v) = patch.agent_tool_id {
            if let Some(tool_id) = v {
                self.fetch_agent_tool(project, tool_id)
                    .map_err(|e| match e {
                        CoreError::AgentToolNotFound(_) => CoreError::BadRequest(format!(
                            "agent_tool_id {tool_id} does not exist in this project"
                        )),
                        other => other,
                    })?;
            }
            entry.agent_tool_id = v;
        }
        if let Some(v) = patch.pid {
            entry.pid = v;
        }
        if let Some(v) = patch.pane_id {
            entry.pane_id = v;
        }
        if let Some(v) = patch.status {
            entry.status = v;
        }
        if let Some(v) = patch.agent_state {
            entry.agent_state = v;
        }
        if let Some(v) = patch.last_seen {
            entry.last_seen = v;
        }
        if let Some(v) = patch.state_since {
            entry.state_since = v;
        }
        self.conn.execute(
            "UPDATE processes
                SET name = ?1, display_name = ?2, command = ?3, cwd = ?4,
                    position = ?5, agent_tool_id = ?6, pid = ?7, pane_id = ?8,
                    status = ?9, agent_state = ?10, last_seen = ?11,
                    state_since = ?12
              WHERE project_id = ?13 AND id = ?14",
            params![
                entry.name,
                entry.display_name,
                entry.command,
                entry.cwd,
                entry.position,
                entry.agent_tool_id.map(|v| v as i64),
                entry.pid,
                entry.pane_id,
                entry.status,
                entry.agent_state,
                entry.last_seen,
                entry.state_since,
                project.0,
                id as i64,
            ],
        )?;
        self.reproject_processes(project)
    }

    /// Soft-delete a process: stamp `deleted_at` and re-project so the row
    /// drops out of the live listing while staying behind for future undelete.
    pub fn process_delete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE processes SET deleted_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::ProcessNotFound(id));
        }
        self.reproject_processes(project)
    }

    /// Start an instance of the agent config `agent_tool_id`: the daemon half
    /// of the lifecycle (todo #141). Writes the desired-state row that a
    /// reconciler (the cockpit plugin) turns into a live pane - the daemon
    /// never spawns the pane itself, so this is pure record-keeping.
    ///
    /// The config must exist and be enabled. This is a **pure factory** (todo
    /// #190): every call spawns a fresh instance, so one config can back many
    /// concurrent instances. Each row is written in [`process_status::STARTING`],
    /// copying the config's command/cwd at spawn time (copy-on-spawn, DESIGN
    /// S6.6) so later edits to the config do not perturb the running instance.
    /// The instance's `name`/`display_name` are uniquified with the new id
    /// (`{config.name}-{id}` / `{friendly} #{id}`) so concurrent instances of
    /// one config never collide in the agent registry or advisory locks.
    /// "Focus the existing one" is a cockpit/UI concern, not a daemon policy.
    pub fn process_start(
        &mut self,
        project: ProjectId,
        agent_tool_id: u64,
    ) -> Result<Process, CoreError> {
        self.process_start_with(project, agent_tool_id, None, Vec::new())
    }

    /// [`Store::process_start`] with the orchestration spawn surface's per-launch
    /// overrides (todo #159): `name` sets the friendly base of the instance's
    /// display name for this run only (still suffixed with the new id, so two
    /// spawns sharing one override stay distinguishable), and `extra_args` are
    /// recorded on the row for the edge to append to the rendered argv. Both are
    /// *copy-on-spawn* - written onto the new `processes` row, never back to the
    /// `agent_tools` config - so a second spawn of the same config with different
    /// overrides leaves the config (and the first instance) untouched.
    ///
    /// Like [`Store::process_start`] this is a pure factory (todo #190): every
    /// call writes a fresh row, never returning an existing one.
    pub fn process_start_with(
        &mut self,
        project: ProjectId,
        agent_tool_id: u64,
        name: Option<String>,
        extra_args: Vec<String>,
    ) -> Result<Process, CoreError> {
        let config = self.fetch_agent_tool(project, agent_tool_id)?;
        if !config.enabled {
            return Err(CoreError::BadRequest(format!(
                "agent config #{agent_tool_id} is disabled"
            )));
        }
        // The friendly display base: the per-launch override when given, else
        // the config's own display name (copy-on-spawn). A blank override falls
        // through to the config so an empty string never blanks the row. The new
        // instance id is appended below so concurrent instances stay distinct.
        let friendly = name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(config.display_name);
        let extra_args_json =
            serde_json::to_string(&extra_args).unwrap_or_else(|_| "[]".to_string());

        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid)?;
            // Uniquify identity with the just-allocated id (todo #190): the
            // internal `name` is the agent's registry/lock key, so two instances
            // of one config must differ. A blank config name falls back to
            // `agent-<id>` (matching the edge and the ad-hoc spawn path).
            let instance_name = if config.name.trim().is_empty() {
                format!("agent-{next}")
            } else {
                format!("{}-{next}", config.name)
            };
            // The display gets a `#<id>` suffix so the cockpit can tell instances
            // apart; a blank friendly base falls back to the internal name rather
            // than rendering a bare " #<id>".
            let instance_display = if friendly.trim().is_empty() {
                instance_name.clone()
            } else {
                format!("{friendly} #{next}")
            };
            tx.execute(
                "INSERT INTO processes
                    (project_id, id, kind, name, display_name, command, cwd,
                     position, agent_tool_id, status, created_at, extra_args)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, datetime('now'), ?11)",
                params![
                    pid,
                    next,
                    ProcessKind::Agent.as_str(),
                    instance_name,
                    instance_display,
                    config.command,
                    config.cwd,
                    next,
                    agent_tool_id as i64,
                    process_status::STARTING,
                    extra_args_json,
                ],
            )?;
            tx.execute(
                "UPDATE projects SET next_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_processes(project)?;
        self.fetch_process(project, id)
    }

    /// Render the bootstrap `agent_instructions` for instance `process` (todo
    /// #159): the spawn surface's reply that an orchestrator prepends to a
    /// spawned child's first prompt. `None` when the instance has no backing
    /// config, the config's type has no profile, or the profile declares no
    /// `instructions`.
    ///
    /// `host`/`port`/`token`/`panopt_bin` come from the daemon (the Store does
    /// not own the listener config); `ws`/`project`/`process_id`/`name`/`model`
    /// are sourced here from the project record, the row, and the profile - so
    /// the renderer sees the same facts the edge will when it builds the launch.
    pub fn render_agent_instructions(
        &self,
        project: ProjectId,
        process: &Process,
        host: &str,
        port: u16,
        token: &str,
        panopt_bin: &str,
    ) -> Result<Option<String>, CoreError> {
        let Some(tool_id) = process.agent_tool_id else {
            return Ok(None);
        };
        let config = self.fetch_agent_tool(project, tool_id)?;
        let Some(profile) = self.profiles.get(&config.tool_type) else {
            return Ok(None);
        };
        let root = self.project_root(project)?;
        let identity: String = self.conn.query_row(
            "SELECT identity FROM projects WHERE id = ?1",
            [project.0],
            |r| r.get(0),
        )?;
        let agent_id = if process.name.trim().is_empty() {
            config.name.clone()
        } else {
            process.name.clone()
        };
        let name = if process.display_name.trim().is_empty() {
            agent_id.clone()
        } else {
            process.display_name.clone()
        };
        let facts = Facts {
            panopt_bin: panopt_bin.to_string(),
            host: host.to_string(),
            port,
            ws: root.to_string_lossy().into_owned(),
            project: identity,
            agent_id,
            name,
            token: token.to_string(),
            model: profile.default_model.clone(),
            process_id: Some(process.id),
        };
        render_instructions(profile, &facts).map_err(|e| CoreError::BadRequest(e.to_string()))
    }

    /// Report runtime facts for an instance from its executing host (todo
    /// #141): the edge wrapper reports its `pid`, the cockpit plugin reports
    /// the `pane_id` it landed the process in. Each is optional so the two
    /// reporters can call independently and idempotently. Setting a `pid`
    /// flips the row to [`process_status::RUNNING`] and stamps `last_seen` -
    /// the pid is the liveness anchor, so its arrival is what means "live".
    pub fn process_report(
        &mut self,
        project: ProjectId,
        id: u64,
        pid: Option<i64>,
        pane_id: Option<String>,
        agent_state: Option<String>,
    ) -> Result<(), CoreError> {
        let mut patch = ProcessPatch {
            pane_id: pane_id.map(Some),
            agent_state: agent_state.clone().map(Some),
            ..Default::default()
        };
        if let Some(pid) = pid {
            patch.pid = Some(Some(pid));
            patch.status = Some(Some(process_status::RUNNING.to_string()));
            patch.last_seen = Some(Some(now_text(&self.conn)?));
        }
        // A genuine state change stamps `state_since` (bug #163), so the
        // projection can render "time in this state". Re-reporting the same
        // state leaves the clock alone (the observer only reports on change,
        // but a periodic re-report must not reset the age either).
        if let Some(new_state) = &agent_state {
            let current = self.fetch_process(project, id)?;
            if current.agent_state.as_deref() != Some(new_state.as_str()) {
                patch.state_since = Some(Some(now_epoch_secs()));
            }
        }
        self.process_update(project, id, patch)
    }

    /// Stop an instance (todo #141): flip its status to
    /// [`process_status::STOPPED`] and return its pid so the caller (the
    /// daemon, which lives on the executing host in the co-located case) can
    /// signal the process. Core itself sends no OS signal - that host effect
    /// belongs to the daemon, not the state layer.
    ///
    /// The pane is deliberately left untouched: panopt owns *processes*,
    /// Zellij and the user own *panes* (the plugin never closes a pane). The
    /// stopped row keeps its pid for the record; the returned pid is what to
    /// signal.
    pub fn process_stop(&mut self, project: ProjectId, id: u64) -> Result<Option<i64>, CoreError> {
        let entry = self.fetch_process(project, id)?;
        self.process_update(
            project,
            id,
            ProcessPatch {
                status: Some(Some(process_status::STOPPED.to_string())),
                ..Default::default()
            },
        )?;
        Ok(entry.pid)
    }

    /// Enqueue a line of input for instance `process_id` (todo #160): the daemon
    /// half of `send_input`. The process must exist; the row is appended to the
    /// `process_inputs` queue and the queue re-projected so the cockpit plugin
    /// can pick it up, write it into the owned pane, and ack it. Core writes no
    /// keystrokes itself - that host effect belongs to the plugin.
    pub fn send_input(
        &mut self,
        project: ProjectId,
        process_id: u64,
        content: &str,
    ) -> Result<(), CoreError> {
        // The target must exist (not soft-deleted) and be live - `starting` or
        // `running`. `starting` is allowed so an ad-hoc spawn's opening prompt
        // can queue before the pane is up (the plugin delivers once running); a
        // `stopped`/exited instance is rejected so input can't pile up for a
        // process that will never receive it.
        let process = self.fetch_process(project, process_id)?;
        if !matches!(
            process.status.as_deref(),
            Some(process_status::STARTING) | Some(process_status::RUNNING)
        ) {
            return Err(CoreError::BadRequest(format!(
                "process #{process_id} is not live (cannot receive input)"
            )));
        }
        self.conn.execute(
            "INSERT INTO process_inputs (project_id, process_id, content, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![project.0, process_id as i64, content],
        )?;
        self.reproject_inputs(project)
    }

    /// The project's undelivered queued inputs, oldest first (todo #160). The
    /// projection layer renders these to `.panopt/.cockpit/inputs.jsonl`.
    pub fn pending_inputs(&self, project: ProjectId) -> Result<Vec<PendingInput>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, process_id, content
               FROM process_inputs
              WHERE project_id = ?1 AND delivered_at IS NULL
              ORDER BY id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            Ok(PendingInput {
                id: r.get(0)?,
                process_id: r.get::<_, i64>(1)? as u64,
                content: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Mark queued input `seq` delivered (todo #160): the cockpit plugin calls
    /// this after it has written the content into the pane, so the daemon drops
    /// it from the projection. Idempotent - acking an already-delivered or
    /// unknown id is a no-op (the keystrokes are not replayed).
    pub fn ack_input(&mut self, project: ProjectId, seq: i64) -> Result<(), CoreError> {
        self.conn.execute(
            "UPDATE process_inputs SET delivered_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND delivered_at IS NULL",
            params![project.0, seq],
        )?;
        self.reproject_inputs(project)
    }

    /// Re-render `.panopt/.cockpit/inputs.jsonl` from the undelivered queue.
    fn reproject_inputs(&mut self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        let pending = self.pending_inputs(project)?;
        projection::project_inputs(&root, &pending)?;
        Ok(())
    }

    fn fetch_process(&self, project: ProjectId, id: u64) -> Result<Process, CoreError> {
        self.conn
            .query_row(
                "SELECT kind, name, display_name, command, cwd, position,
                        agent_tool_id, pid, pane_id, status, agent_state, last_seen,
                        state_since, created_at, extra_args
                   FROM processes
                  WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![project.0, id as i64],
                |r| {
                    let kind: String = r.get(0)?;
                    Ok(Process {
                        id,
                        kind: ProcessKind::parse(&kind).unwrap_or_default(),
                        name: r.get(1)?,
                        display_name: r.get(2)?,
                        command: r.get(3)?,
                        cwd: r.get(4)?,
                        position: r.get(5)?,
                        agent_tool_id: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                        pid: r.get(7)?,
                        pane_id: r.get(8)?,
                        status: r.get(9)?,
                        agent_state: r.get(10)?,
                        last_seen: r.get(11)?,
                        state_since: r.get(12)?,
                        created_at: r.get(13)?,
                        extra_args: parse_extra_args(&r.get::<_, String>(14)?),
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::ProcessNotFound(id))
    }

    // --- todos ---

    /// Create a new open todo in `project` and return its id.
    pub fn todo_create(&mut self, project: ProjectId, title: String) -> Result<u64, CoreError> {
        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid)?;
            tx.execute(
                "INSERT INTO todos (project_id, id, title, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'open', datetime('now'), datetime('now'))",
                params![pid, next, title],
            )?;
            tx.execute(
                "UPDATE projects SET next_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_todos(project)?;
        Ok(id)
    }

    /// List a project's todos in full - blockers and comments included -
    /// id-ascending.
    pub fn todo_list(&self, project: ProjectId) -> Result<Vec<Todo>, CoreError> {
        let ids: Vec<u64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM todos
                  WHERE project_id = ?1 AND deleted_at IS NULL ORDER BY id",
            )?;
            let rows = stmt.query_map([project.0], |r| Ok(r.get::<_, i64>(0)? as u64))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        ids.into_iter()
            .map(|id| self.fetch_todo(project, id))
            .collect()
    }

    /// Search a project's todos, returning the same fully-hydrated
    /// [`Todo`] shape as [`Store::todo_list`].
    ///
    /// `query` substring-matches `title`/`body` case-insensitively at the SQL
    /// layer; a purely numeric query (optionally `#`-prefixed) *also* matches
    /// the exact id, so a `#N` reference is reachable from search (see
    /// [`query_as_id`]). `status`, `priority`, and `assignee` are equality
    /// predicates (assignee is case-insensitive, the others compare against the
    /// canonical token from [`TodoStatus::as_str`] / [`Priority::as_str`]); and
    /// `require_tags` is applied in Rust after hydration (AND semantics),
    /// since tags are stored as a JSON-encoded column. With every filter
    /// absent or empty this matches `todo_list` exactly.
    pub fn todo_search(
        &self,
        project: ProjectId,
        query: Option<&str>,
        status: Option<TodoStatus>,
        priority: Option<Priority>,
        assignee: Option<&str>,
        require_tags: &[String],
    ) -> Result<Vec<Todo>, CoreError> {
        let mut sql = String::from(
            "SELECT id FROM todos \
             WHERE project_id = ?1 AND deleted_at IS NULL",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(project.0)];
        if let Some(q) = query {
            let pat = format!("%{}%", q.to_lowercase());
            let n = binds.len() + 1;
            binds.push(Box::new(pat));
            let mut clause = format!("LOWER(title) LIKE ?{n} OR LOWER(body) LIKE ?{n}");
            if let Some(id) = query_as_id(q) {
                let m = binds.len() + 1;
                binds.push(Box::new(id as i64));
                clause.push_str(&format!(" OR id = ?{m}"));
            }
            sql.push_str(&format!(" AND ({clause})"));
        }
        if let Some(s) = status {
            sql.push_str(&format!(" AND status = ?{}", binds.len() + 1));
            binds.push(Box::new(s.as_str().to_string()));
        }
        if let Some(p) = priority {
            sql.push_str(&format!(" AND priority = ?{}", binds.len() + 1));
            binds.push(Box::new(p.as_str().to_string()));
        }
        if let Some(a) = assignee {
            sql.push_str(&format!(
                " AND LOWER(assignee) = LOWER(?{})",
                binds.len() + 1
            ));
            binds.push(Box::new(a.to_string()));
        }
        sql.push_str(" ORDER BY id");

        let ids: Vec<u64> = {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(
                rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
                |r| Ok(r.get::<_, i64>(0)? as u64),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let todos: Vec<Todo> = ids
            .into_iter()
            .map(|id| self.fetch_todo(project, id))
            .collect::<Result<Vec<_>, _>>()?;
        if require_tags.is_empty() {
            Ok(todos)
        } else {
            Ok(todos
                .into_iter()
                .filter(|t| {
                    require_tags
                        .iter()
                        .all(|wanted| t.tags.iter().any(|got| got == wanted))
                })
                .collect())
        }
    }

    /// Fetch one todo in full, or [`CoreError::TodoNotFound`] if it is absent.
    pub fn todo_get(&self, project: ProjectId, id: u64) -> Result<Todo, CoreError> {
        self.fetch_todo(project, id)
    }

    /// Apply a [`TodoPatch`]: every `Some` field is written, every `None` field
    /// is left as-is. `updated_at` is always bumped, and `completed_at` is
    /// reconciled with the resulting status.
    pub fn todo_update(
        &mut self,
        project: ProjectId,
        id: u64,
        patch: TodoPatch,
    ) -> Result<(), CoreError> {
        let mut todo = self.fetch_todo(project, id)?;
        if let Some(v) = patch.title {
            todo.title = v;
        }
        if let Some(v) = patch.body {
            todo.body = v;
        }
        if let Some(v) = patch.status {
            todo.status = v;
        }
        if let Some(v) = patch.priority {
            todo.priority = v;
        }
        if let Some(v) = patch.assignee {
            todo.assignee = v;
        }
        if let Some(v) = patch.tags {
            todo.tags = v;
        }
        let tags_json = serde_json::to_string(&todo.tags).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "UPDATE todos
                SET title = ?1, body = ?2, status = ?3, priority = ?4,
                    assignee = ?5, tags = ?6, updated_at = datetime('now')
              WHERE project_id = ?7 AND id = ?8",
            params![
                todo.title,
                todo.body,
                todo.status.as_str(),
                todo.priority.as_str(),
                todo.assignee,
                tags_json,
                project.0,
                id as i64,
            ],
        )?;
        self.reconcile_completed_at(project, id, todo.status)?;
        self.reproject_todos(project)
    }

    /// Mark a todo complete. Idempotent: completing an already-done todo
    /// succeeds and re-projects.
    pub fn todo_complete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE todos SET status = 'completed', updated_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::TodoNotFound(id));
        }
        self.reconcile_completed_at(project, id, TodoStatus::Completed)?;
        self.reproject_todos(project)
    }

    /// Transition a todo to [`TodoStatus::InProgress`] to signal that an agent
    /// has begun work. Idempotent when the todo is already in progress.
    /// Terminal states (`Completed`, `NotDone`) are rejected as
    /// [`CoreError::BadRequest`] so the caller has to reopen the todo
    /// explicitly via [`Self::todo_update`] - silently un-terminating a closed
    /// todo would lose the close signal.
    pub fn todo_start(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let todo = self.fetch_todo(project, id)?;
        match todo.status {
            TodoStatus::InProgress => return Ok(()),
            TodoStatus::Completed | TodoStatus::NotDone => {
                return Err(CoreError::BadRequest(format!(
                    "todo {id} is {} - reopen via todo_update before starting",
                    todo.status.as_str()
                )));
            }
            TodoStatus::Open | TodoStatus::Backlog | TodoStatus::Draft => {}
        }
        self.conn.execute(
            "UPDATE todos SET status = 'in_progress', updated_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![project.0, id as i64],
        )?;
        self.reconcile_completed_at(project, id, TodoStatus::InProgress)?;
        self.reproject_todos(project)
    }

    /// Soft-delete a todo: stamp `deleted_at` so the live list, fetches, and
    /// the projection sweep it. Comments and blocker links stay in their side
    /// tables for the eventual undelete to reattach; cleanup of orphan side
    /// rows is deferred (see todo #54).
    pub fn todo_delete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE todos SET deleted_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::TodoNotFound(id));
        }
        self.reproject_todos(project)
    }

    /// Record that todo `id` is blocked by `blocker_id`. Both todos must exist
    /// and be distinct. Idempotent: an already-recorded blocker is left as-is.
    pub fn todo_add_blocker(
        &mut self,
        project: ProjectId,
        id: u64,
        blocker_id: u64,
    ) -> Result<(), CoreError> {
        if id == blocker_id {
            return Err(CoreError::BadRequest("a todo cannot block itself".into()));
        }
        self.fetch_todo(project, id)?;
        self.fetch_todo(project, blocker_id)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO todo_blockers (project_id, todo_id, blocker_id)
             VALUES (?1, ?2, ?3)",
            params![project.0, id as i64, blocker_id as i64],
        )?;
        self.touch_todo(project, id)?;
        self.reproject_todos(project)
    }

    /// Remove the record that todo `id` is blocked by `blocker_id`. Idempotent:
    /// a blocker that was not recorded is a no-op. The blocked todo must exist.
    pub fn todo_remove_blocker(
        &mut self,
        project: ProjectId,
        id: u64,
        blocker_id: u64,
    ) -> Result<(), CoreError> {
        self.fetch_todo(project, id)?;
        self.conn.execute(
            "DELETE FROM todo_blockers
              WHERE project_id = ?1 AND todo_id = ?2 AND blocker_id = ?3",
            params![project.0, id as i64, blocker_id as i64],
        )?;
        self.touch_todo(project, id)?;
        self.reproject_todos(project)
    }

    /// Append a comment to a todo and return the new comment's id (unique
    /// within that todo, restarting at 1 in each todo).
    pub fn todo_comment_add(
        &mut self,
        project: ProjectId,
        id: u64,
        author: String,
        body: String,
    ) -> Result<u64, CoreError> {
        let pid = project.0;
        let comment_id = {
            let tx = self.conn.transaction()?;
            let next: Option<i64> = tx
                .query_row(
                    "SELECT next_comment_id FROM todos
                      WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                    params![pid, id as i64],
                    |r| r.get(0),
                )
                .optional()?;
            let next = next.ok_or(CoreError::TodoNotFound(id))?;
            tx.execute(
                "INSERT INTO todo_comments (project_id, todo_id, id, author, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                params![pid, id as i64, next, author, body],
            )?;
            tx.execute(
                "UPDATE todos SET next_comment_id = ?1, updated_at = datetime('now')
                  WHERE project_id = ?2 AND id = ?3",
                params![next + 1, pid, id as i64],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_todos(project)?;
        Ok(comment_id)
    }

    /// Replace the body of an existing comment. The author and `created_at`
    /// timestamp are preserved - a comment edit is not a re-post.
    pub fn todo_comment_update(
        &mut self,
        project: ProjectId,
        todo_id: u64,
        comment_id: u64,
        body: String,
    ) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE todo_comments SET body = ?1
              WHERE project_id = ?2 AND todo_id = ?3 AND id = ?4",
            params![body, project.0, todo_id as i64, comment_id as i64],
        )?;
        if changed == 0 {
            // The comment row is missing; surface whichever id is at fault.
            self.fetch_todo(project, todo_id)?;
            return Err(CoreError::TodoCommentNotFound {
                todo_id,
                comment_id,
            });
        }
        self.touch_todo(project, todo_id)?;
        self.reproject_todos(project)
    }

    /// Remove a comment from a todo. The comment id is **not** reused - the
    /// per-todo `next_comment_id` counter keeps advancing, so a later
    /// `todo_comment_add` lands at the next fresh id.
    pub fn todo_comment_delete(
        &mut self,
        project: ProjectId,
        todo_id: u64,
        comment_id: u64,
    ) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "DELETE FROM todo_comments
              WHERE project_id = ?1 AND todo_id = ?2 AND id = ?3",
            params![project.0, todo_id as i64, comment_id as i64],
        )?;
        if changed == 0 {
            self.fetch_todo(project, todo_id)?;
            return Err(CoreError::TodoCommentNotFound {
                todo_id,
                comment_id,
            });
        }
        self.touch_todo(project, todo_id)?;
        self.reproject_todos(project)
    }

    /// Replace a todo's blocker set with `blockers` in one transactional step.
    /// Convenience over the per-id `add`/`remove` calls, which the form uses
    /// during debounced autosave to avoid a half-applied state.
    pub fn todo_set_blockers(
        &mut self,
        project: ProjectId,
        id: u64,
        blockers: Vec<u64>,
    ) -> Result<(), CoreError> {
        // Reject self-blocking up front so the diff loop does not silently skip it.
        if blockers.contains(&id) {
            return Err(CoreError::BadRequest("a todo cannot block itself".into()));
        }
        // Make sure the blocked todo and every requested blocker exists before
        // mutating anything; otherwise a half-applied set leaks on error.
        self.fetch_todo(project, id)?;
        for b in &blockers {
            self.fetch_todo(project, *b)?;
        }
        let desired: HashSet<u64> = blockers.into_iter().collect();
        let current: HashSet<u64> = self.todo_blockers(project, id)?.into_iter().collect();
        for &remove in current.difference(&desired) {
            self.conn.execute(
                "DELETE FROM todo_blockers
                  WHERE project_id = ?1 AND todo_id = ?2 AND blocker_id = ?3",
                params![project.0, id as i64, remove as i64],
            )?;
        }
        for &add in desired.difference(&current) {
            self.conn.execute(
                "INSERT OR IGNORE INTO todo_blockers (project_id, todo_id, blocker_id)
                 VALUES (?1, ?2, ?3)",
                params![project.0, id as i64, add as i64],
            )?;
        }
        self.touch_todo(project, id)?;
        self.reproject_todos(project)
    }

    /// The sorted, deduped union of every tag used by any todo *or* note
    /// in `project`. The two surfaces share one project-wide vocabulary (todo
    /// #61) so a tag picked on one offers up on the other; this is the single
    /// source of truth behind both `todo_tags_list` and `note_tags_list`
    /// MCP tools, which return identical output.
    pub fn tags_list(&self, project: ProjectId) -> Result<Vec<String>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT tags FROM todos       WHERE project_id = ?1 AND deleted_at IS NULL
             UNION ALL
             SELECT tags FROM notes WHERE project_id = ?1 AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map([project.0], |r| r.get::<_, String>(0))?;
        let mut set: HashSet<String> = HashSet::new();
        for row in rows {
            let json = row?;
            // A bad tag JSON blob is treated as empty rather than aborting the
            // whole list - the form should not bomb out on a stray write.
            let tags: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            for t in tags {
                if !t.is_empty() {
                    set.insert(t);
                }
            }
        }
        let mut tags: Vec<String> = set.into_iter().collect();
        tags.sort();
        Ok(tags)
    }

    /// Back-compat alias for [`Self::tags_list`]. Both `todo_tags_list` and
    /// `note_tags_list` MCP tools call the unified method now (todo #61),
    /// so this just exists so callsites that already imported the todo-only
    /// name keep compiling.
    pub fn todo_tags_list(&self, project: ProjectId) -> Result<Vec<String>, CoreError> {
        self.tags_list(project)
    }

    /// Set or clear `completed_at` to match `status`: a `Completed` todo keeps
    /// any existing timestamp or gets one now; any other status clears it.
    fn reconcile_completed_at(
        &self,
        project: ProjectId,
        id: u64,
        status: TodoStatus,
    ) -> Result<(), CoreError> {
        let sql = if status == TodoStatus::Completed {
            "UPDATE todos SET completed_at = COALESCE(completed_at, datetime('now'))
              WHERE project_id = ?1 AND id = ?2"
        } else {
            "UPDATE todos SET completed_at = NULL WHERE project_id = ?1 AND id = ?2"
        };
        self.conn.execute(sql, params![project.0, id as i64])?;
        Ok(())
    }

    /// Bump a todo's `updated_at` to now. Used by mutations of a todo's side
    /// tables, which do not otherwise touch the `todos` row.
    fn touch_todo(&self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        self.conn.execute(
            "UPDATE todos SET updated_at = datetime('now') WHERE project_id = ?1 AND id = ?2",
            params![project.0, id as i64],
        )?;
        Ok(())
    }

    /// Fetch one todo with its blockers and comments, or
    /// [`CoreError::TodoNotFound`] when no such todo exists in `project`.
    fn fetch_todo(&self, project: ProjectId, id: u64) -> Result<Todo, CoreError> {
        let mut todo = self
            .conn
            .query_row(
                "SELECT title, body, status, priority, assignee, tags,
                        created_at, updated_at, completed_at
                   FROM todos
                  WHERE project_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![project.0, id as i64],
                |r| {
                    let status: String = r.get(2)?;
                    let priority: String = r.get(3)?;
                    let tags: String = r.get(5)?;
                    Ok(Todo {
                        id,
                        title: r.get(0)?,
                        body: r.get(1)?,
                        status: TodoStatus::parse(&status).unwrap_or(TodoStatus::Open),
                        priority: Priority::parse(&priority).unwrap_or(Priority::Medium),
                        assignee: r.get(4)?,
                        tags: serde_json::from_str(&tags).unwrap_or_default(),
                        blockers: Vec::new(),
                        comments: Vec::new(),
                        created_at: r.get(6)?,
                        updated_at: r.get(7)?,
                        completed_at: r.get(8)?,
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::TodoNotFound(id))?;
        todo.blockers = self.todo_blockers(project, id)?;
        todo.comments = self.todo_comments(project, id)?;
        Ok(todo)
    }

    /// The ids that block todo `id`, ascending. Soft-deleted blockers are
    /// hidden so the form / projection never surfaces a chip pointing at a
    /// row that the list otherwise treats as gone.
    fn todo_blockers(&self, project: ProjectId, id: u64) -> Result<Vec<u64>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT b.blocker_id FROM todo_blockers b
              WHERE b.project_id = ?1 AND b.todo_id = ?2
                AND EXISTS (
                    SELECT 1 FROM todos t
                     WHERE t.project_id = b.project_id
                       AND t.id = b.blocker_id
                       AND t.deleted_at IS NULL
                )
              ORDER BY b.blocker_id",
        )?;
        let rows = stmt.query_map(params![project.0, id as i64], |r| {
            Ok(r.get::<_, i64>(0)? as u64)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The comments on todo `id`, in post order.
    fn todo_comments(&self, project: ProjectId, id: u64) -> Result<Vec<TodoComment>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, author, body, created_at FROM todo_comments
              WHERE project_id = ?1 AND todo_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![project.0, id as i64], |r| {
            Ok(TodoComment {
                id: r.get::<_, i64>(0)? as u64,
                author: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // --- projection ---

    fn project_root(&self, project: ProjectId) -> Result<PathBuf, CoreError> {
        let root: String = self
            .conn
            .query_row(
                "SELECT root FROM projects WHERE id = ?1",
                [project.0],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(CoreError::ProjectNotFound(project.0))?;
        Ok(PathBuf::from(root))
    }

    fn reproject_todos(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_todos(&root, &self.todo_list(project)?)?;
        Ok(())
    }

    fn reproject_note(&self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_note(&root, &self.fetch_note(project, id)?)?;
        Ok(())
    }

    fn reproject_notes_index(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_notes_index(&root, &self.note_index_rows(project)?)?;
        Ok(())
    }

    /// Rewrite both the per-note file and the note index, so every
    /// mutation that touches a note refreshes the projection completely.
    /// The single helper makes the index step impossible for a caller to skip,
    /// matching the all-or-nothing shape `reproject_todos` already has.
    fn reproject_note_full(&self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        self.reproject_note(project, id)?;
        self.reproject_notes_index(project)
    }

    /// `(id, title, updated_at)` for every note in `project`, id-ascending.
    /// The projection layer renders the `updated_at` into each index line; the
    /// public `note_list` stays the narrow `(id, title)` shape the MCP
    /// wire summary expects.
    fn note_index_rows(&self, project: ProjectId) -> Result<Vec<(u64, String, String)>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, updated_at FROM notes
              WHERE project_id = ?1 AND deleted_at IS NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn reproject_agent_tools(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_agent_tools(&root, &self.agent_tool_list(project)?)?;
        Ok(())
    }

    fn reproject_processes(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_processes(
            &root,
            &self.process_list(project)?,
            &self.tool_type_map(project)?,
            std::time::SystemTime::now(),
        )?;
        Ok(())
    }

    /// `agent_tool_id -> tool_type` for this project, the join the processes
    /// projection needs to tag each agent row with the profile that classifies
    /// it (#142). The type key lives on the config, not copied onto the row.
    fn tool_type_map(
        &self,
        project: ProjectId,
    ) -> Result<std::collections::BTreeMap<u64, String>, CoreError> {
        Ok(self
            .agent_tool_list(project)?
            .into_iter()
            .map(|t| (t.id, t.tool_type))
            .collect())
    }

    fn reproject_agents(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_agents(
            &root,
            &self.registry.list(project.0),
            std::time::SystemTime::now(),
        )?;
        Ok(())
    }

    fn reproject_locks(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_locks(&root, &self.lock_list(project))?;
        Ok(())
    }

    /// Re-project every file of a project from current state. Run once per
    /// project per process by [`Self::ensure_project`].
    fn reproject_all(&self, root: &Path, project: ProjectId) -> Result<(), CoreError> {
        projection::project_todos(root, &self.todo_list(project)?)?;
        projection::project_agent_tools(root, &self.agent_tool_list(project)?)?;
        projection::project_processes(
            root,
            &self.process_list(project)?,
            &self.tool_type_map(project)?,
            std::time::SystemTime::now(),
        )?;
        if let Ok(profiles) = crate::agent_profiles::ProfileSet::load() {
            projection::project_agent_types(root, &profiles)?;
        }
        projection::project_agents(
            root,
            &self.registry.list(project.0),
            std::time::SystemTime::now(),
        )?;
        projection::project_locks(root, &self.lock_list(project))?;
        projection::project_notes_index(root, &self.note_index_rows(project)?)?;
        for (id, _) in self.note_list(project)? {
            projection::project_note(root, &self.fetch_note(project, id)?)?;
        }
        Ok(())
    }
}

/// A search query that is just a number (optionally prefixed with `#`, e.g.
/// `119` or `#119`) is treated as an exact id lookup against the unified
/// per-project id, so a `#N` reference is reachable from search. Non-numeric
/// queries return `None` and fall through to plain title/body matching.
fn query_as_id(query: &str) -> Option<u64> {
    let trimmed = query.trim();
    trimmed.strip_prefix('#').unwrap_or(trimmed).parse().ok()
}

/// Read a project's global `next_id` counter, mapping a missing project row to
/// [`CoreError::ProjectNotFound`]. The counter is shared across todos,
/// notes, agent tools, and processes, so a `#N` reference is unambiguous.
fn next_id(conn: &Connection, pid: i64) -> Result<i64, CoreError> {
    conn.query_row("SELECT next_id FROM projects WHERE id = ?1", [pid], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or(CoreError::ProjectNotFound(pid))
}

/// SQLite's `datetime('now')` as a string. Used when a timestamp must be bound
/// as a parameter (e.g. a [`ProcessPatch`] field) rather than written inline in
/// the SQL, so it matches the `YYYY-MM-DD HH:MM:SS` UTC format every other
/// `created_at`/`updated_at` column already stores.
fn now_text(conn: &Connection) -> Result<String, CoreError> {
    Ok(conn.query_row("SELECT datetime('now')", [], |r| r.get(0))?)
}

/// Wall-clock now as Unix-epoch seconds, for the `state_since` stamp (bug #163).
/// Stored as an integer (not the `datetime('now')` TEXT the other timestamps
/// use) so the processes projection can diff it against its render clock without
/// parsing a date string. A clock before the epoch clamps to 0.
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse the `processes.extra_args` JSON-array column (todo #159) into the
/// instance's per-launch argument list. A malformed or legacy value reads back
/// as empty rather than erroring the whole row - the args are an additive launch
/// detail, not load-bearing state.
fn parse_extra_args(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Store` on a throwaway database, plus the temp dir its projects and
    /// database file live in. Field order matters: `store` drops (closing the
    /// connection) before `dir` drops (deleting the files).
    struct Fixture {
        store: Store,
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(&dir.path().join("panopt.db")).unwrap();
            Fixture { store, dir }
        }

        /// Create a project directory under the temp dir and register it.
        fn project(&mut self, name: &str) -> (ProjectId, PathBuf) {
            let root = self.dir.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            let id = self.store.ensure_project(&root).unwrap();
            (id, root)
        }
    }

    /// The `identity` column of a project row, read directly for assertions.
    fn identity_of(store: &Store, project: ProjectId) -> String {
        store
            .conn
            .query_row(
                "SELECT identity FROM projects WHERE id = ?1",
                [project.0],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn ensure_project_by_identity_adopts_unifies_and_resists_clobber() {
        let mut fx = Fixture::new();
        let a = fx.dir.path().join("checkout-a");
        std::fs::create_dir_all(&a).unwrap();

        // A path-keyed connect seeds the row with identity == canonical path.
        let p_path = fx.store.ensure_project(&a).unwrap();
        let canon_a = std::fs::canonicalize(&a)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(identity_of(&fx.store, p_path), canon_a);

        // The edge resolver connects with a repo identity for the same path: it
        // adopts the existing row (no UNIQUE(root) collision) and re-keys it.
        let p_id = fx.store.ensure_project_by_identity("repo-key", &a).unwrap();
        assert_eq!(p_path, p_id, "identity connect adopts the path-keyed row");
        assert_eq!(identity_of(&fx.store, p_id), "repo-key");

        // A second checkout at a different path but the same identity unifies
        // onto the same project row - the whole point of repo identity.
        let b = fx.dir.path().join("checkout-b");
        std::fs::create_dir_all(&b).unwrap();
        let p_other = fx.store.ensure_project_by_identity("repo-key", &b).unwrap();
        assert_eq!(p_id, p_other, "same identity unifies distinct checkouts");

        // A later path-only client (the CLI) must NOT clobber the established
        // repo identity back to the path.
        let p_path2 = fx.store.ensure_project(&a).unwrap();
        assert_eq!(p_id, p_path2);
        assert_eq!(
            identity_of(&fx.store, p_id),
            "repo-key",
            "path call left identity intact"
        );
    }

    #[test]
    fn project_list_joins_aggregates_per_project() {
        let mut fx = Fixture::new();
        let (alpha, _) = fx.project("alpha");
        let (_beta, _) = fx.project("beta");

        // alpha: two open todos, one in-progress, plus a backlog todo that must
        // not be counted; a note; a held lock; and a live agent.
        let _o1 = fx.store.todo_create(alpha, "open one".into()).unwrap();
        let _o2 = fx.store.todo_create(alpha, "open two".into()).unwrap();
        let ip = fx.store.todo_create(alpha, "wip".into()).unwrap();
        let bl = fx.store.todo_create(alpha, "later".into()).unwrap();
        fx.store
            .todo_update(
                alpha,
                ip,
                TodoPatch {
                    status: Some(TodoStatus::InProgress),
                    ..Default::default()
                },
            )
            .unwrap();
        fx.store
            .todo_update(
                alpha,
                bl,
                TodoPatch {
                    status: Some(TodoStatus::Backlog),
                    ..Default::default()
                },
            )
            .unwrap();
        fx.store.note_create(alpha, "a note".into()).unwrap();
        fx.store
            .lock_acquire(alpha, "agent-1", "build".into(), None)
            .unwrap();
        fx.store
            .agent_touch(alpha, "agent-1", KeySource::Declared)
            .unwrap();

        let rows = fx.store.project_list().unwrap();
        assert_eq!(rows.len(), 2, "one row per project");

        // Rows come back in project-id (insertion) order: alpha then beta.
        let a = &rows[0];
        assert_eq!(a.name, "alpha");
        assert_eq!(a.todos_open, 2, "backlog todo is not an open badge");
        assert_eq!(a.todos_in_progress, 1);
        assert_eq!(a.locks_held, 1);
        assert_eq!(a.agents_active, 1);
        assert!(
            a.last_activity.is_some(),
            "a project with todos/notes has a last_activity timestamp"
        );
        assert_eq!(a.identity, a.root, "path-keyed project: identity == root");

        // beta is untouched: every badge is zero and there is no activity.
        let b = &rows[1];
        assert_eq!(b.name, "beta");
        assert_eq!(b.todos_open, 0);
        assert_eq!(b.todos_in_progress, 0);
        assert_eq!(b.locks_held, 0);
        assert_eq!(b.agents_active, 0);
        assert_eq!(b.last_activity, None);
    }

    #[test]
    fn project_list_names_track_repo_identity_not_path() {
        // Two checkouts of one repo identity collapse to a single board row,
        // and its `name` is the basename of the projection path the daemon
        // recorded first - not re-derived per checkout.
        let mut fx = Fixture::new();
        let a = fx.dir.path().join("checkout-a");
        let b = fx.dir.path().join("checkout-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        fx.store.ensure_project_by_identity("repo-key", &a).unwrap();
        fx.store.ensure_project_by_identity("repo-key", &b).unwrap();

        let rows = fx.store.project_list().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "same identity is one row, not one per checkout"
        );
        assert_eq!(rows[0].identity, "repo-key");
        assert_eq!(rows[0].name, "checkout-a");
    }

    #[test]
    fn todo_ids_are_monotonic_within_a_project() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        assert_eq!(fx.store.todo_create(p, "a".into()).unwrap(), 1);
        assert_eq!(fx.store.todo_create(p, "b".into()).unwrap(), 2);
        assert_eq!(fx.store.todo_create(p, "c".into()).unwrap(), 3);
    }

    #[test]
    fn ids_are_globally_unique_across_resource_types() {
        // Todo #16 + #27: one shared sequence so `#N` resolves to exactly one
        // resource. A todo, note, agent tool, and process all draw from
        // the same counter in creation order.
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        assert_eq!(fx.store.todo_create(p, "a".into()).unwrap(), 1);
        assert_eq!(fx.store.note_create(p, "pad".into()).unwrap(), 2);
        assert_eq!(
            fx.store
                .agent_tool_create(
                    p,
                    "claude".into(),
                    String::new(),
                    String::new(),
                    String::new(),
                    "claude-code".into(),
                    String::new(),
                    true,
                )
                .unwrap(),
            3
        );
        assert_eq!(
            fx.store
                .process_create(
                    p,
                    ProcessKind::Command,
                    "build".into(),
                    String::new(),
                    "cargo build".into(),
                    String::new(),
                    None,
                )
                .unwrap(),
            4
        );
        assert_eq!(fx.store.todo_create(p, "b".into()).unwrap(), 5);
        assert_eq!(fx.store.note_create(p, "pad2".into()).unwrap(), 6);
    }

    #[test]
    fn process_create_with_invalid_agent_tool_id_is_rejected() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let err = fx
            .store
            .process_create(
                p,
                ProcessKind::Agent,
                "claude-1".into(),
                String::new(),
                "claude".into(),
                String::new(),
                Some(42),
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::BadRequest(_)), "{err:?}");
    }

    #[test]
    fn process_create_with_valid_agent_tool_id_links_it() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let tool = fx
            .store
            .agent_tool_create(
                p,
                "claude".into(),
                String::new(),
                "claude".into(),
                String::new(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let proc = fx
            .store
            .process_create(
                p,
                ProcessKind::Agent,
                "claude-1".into(),
                String::new(),
                "claude".into(),
                String::new(),
                Some(tool),
            )
            .unwrap();
        let row = fx.store.process_get(p, proc).unwrap();
        assert_eq!(row.agent_tool_id, Some(tool));
    }

    #[test]
    fn deleting_an_agent_tool_preserves_the_processes_back_reference() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let tool = fx
            .store
            .agent_tool_create(
                p,
                "claude".into(),
                String::new(),
                "claude".into(),
                String::new(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let proc = fx
            .store
            .process_create(
                p,
                ProcessKind::Agent,
                "claude-1".into(),
                String::new(),
                "claude".into(),
                String::new(),
                Some(tool),
            )
            .unwrap();
        fx.store.agent_tool_delete(p, tool).unwrap();
        // Soft delete keeps the link: the process row still names its source
        // tool, even though `agent_tool_get` now returns NotFound. That way a
        // future undelete reconstitutes the relationship as it was.
        let row = fx.store.process_get(p, proc).unwrap();
        assert_eq!(row.agent_tool_id, Some(tool));
        assert!(matches!(
            fx.store.agent_tool_get(p, tool).unwrap_err(),
            CoreError::AgentToolNotFound(_)
        ));
    }

    #[test]
    fn append_concatenates_with_single_newline() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.note_create(p, "notes".into()).unwrap();
        fx.store.note_append(p, id, "first").unwrap();
        fx.store.note_append(p, id, "second").unwrap();
        assert_eq!(fx.store.note_read(p, id).unwrap(), "first\nsecond");
    }

    #[test]
    fn note_create_sets_created_and_updated_timestamps() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.note_create(p, "notes".into()).unwrap();
        let pad = fx.store.fetch_note(p, id).unwrap();
        assert!(!pad.created_at.is_empty(), "created_at is set on create");
        assert!(!pad.updated_at.is_empty(), "updated_at is set on create");
    }

    #[test]
    fn note_append_bumps_updated_at_and_rewrites_index() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.note_create(p, "notes".into()).unwrap();
        let before = fx.store.fetch_note(p, id).unwrap().updated_at;

        // datetime('now') has 1-second resolution, so cross a second boundary
        // to be sure the timestamp moves; the same constraint todos face.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fx.store.note_append(p, id, "more").unwrap();

        let after = fx.store.fetch_note(p, id).unwrap().updated_at;
        assert!(after > before, "updated_at must advance on append");

        // And the index file now carries the new timestamp - the bytes change
        // so the cockpit's 1s file poller observes the refresh.
        let index = std::fs::read_to_string(root.join(".panopt/notes.md")).unwrap();
        assert!(
            index.contains(&format!("updated {after}")),
            "index reflects the bumped updated_at\n{index}",
        );
    }

    #[test]
    fn note_update_writes_only_the_some_fields() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.note_create(p, "first-title".into()).unwrap();
        fx.store.note_append(p, id, "first-body").unwrap();

        // Patching only title leaves the body alone.
        fx.store
            .note_update(
                p,
                id,
                NotePatch {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let pad = fx.store.note_get(p, id).unwrap();
        assert_eq!(pad.title, "renamed");
        assert_eq!(pad.body, "first-body");

        // Patching only body leaves the title alone.
        fx.store
            .note_update(
                p,
                id,
                NotePatch {
                    body: Some("rewritten".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let pad = fx.store.note_get(p, id).unwrap();
        assert_eq!(pad.title, "renamed");
        assert_eq!(pad.body, "rewritten");
    }

    #[test]
    fn note_update_bumps_updated_at_and_reprojects() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.note_create(p, "notes".into()).unwrap();
        let before = fx.store.note_get(p, id).unwrap().updated_at;

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fx.store
            .note_update(
                p,
                id,
                NotePatch {
                    body: Some("new body".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        let after = fx.store.note_get(p, id).unwrap().updated_at;
        assert!(after > before, "updated_at must advance on update");

        let pad_file = std::fs::read_to_string(root.join(".panopt/note/1.md")).unwrap();
        assert!(
            pad_file.contains("new body"),
            "per-pad projection refreshed"
        );
    }

    #[test]
    fn note_update_errors_when_missing() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let err = fx
            .store
            .note_update(p, 999, NotePatch::default())
            .unwrap_err();
        assert!(matches!(err, CoreError::NoteNotFound(999)));
    }

    #[test]
    fn note_delete_removes_row_and_per_pad_file() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.note_create(p, "deletable-title".into()).unwrap();
        let pad_path = root.join(".panopt/note/1.md");
        assert!(pad_path.exists(), "per-pad file projected on create");

        fx.store.note_delete(p, id).unwrap();

        assert!(!pad_path.exists(), "per-pad file swept on delete");
        assert!(
            fx.store.note_list(p).unwrap().is_empty(),
            "row gone from the listing",
        );

        // The title is distinct from the index chrome ("# Notes", "_(no
        // notes)_") so this only trips when the deleted pad still appears.
        let index = std::fs::read_to_string(root.join(".panopt/notes.md")).unwrap();
        assert!(
            !index.contains("deletable-title"),
            "index no longer lists the pad\n{index}"
        );
    }

    #[test]
    fn note_delete_errors_when_missing() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let err = fx.store.note_delete(p, 999).unwrap_err();
        assert!(matches!(err, CoreError::NoteNotFound(999)));
    }

    #[test]
    fn note_append_rewrites_the_index_even_after_deletion() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.note_create(p, "notes".into()).unwrap();
        // Remove the index to prove `note_append` rewrites it - this
        // is what guards against the pre-fix asymmetry where append skipped
        // the index reprojection entirely.
        std::fs::remove_file(root.join(".panopt/notes.md")).unwrap();
        fx.store.note_append(p, id, "more").unwrap();
        assert!(root.join(".panopt/notes.md").exists());
    }

    #[test]
    fn complete_flips_status_and_is_idempotent() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        assert_eq!(fx.store.todo_list(p).unwrap()[0].status, TodoStatus::Open);
        fx.store.todo_complete(p, id).unwrap();
        let done = fx.store.todo_get(p, id).unwrap();
        assert_eq!(done.status, TodoStatus::Completed);
        assert!(done.completed_at.is_some());
        fx.store.todo_complete(p, id).unwrap(); // idempotent
        assert_eq!(
            fx.store.todo_get(p, id).unwrap().status,
            TodoStatus::Completed
        );
    }

    #[test]
    fn todo_start_flips_status_and_rejects_terminal() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        assert_eq!(fx.store.todo_get(p, id).unwrap().status, TodoStatus::Open);

        fx.store.todo_start(p, id).unwrap();
        assert_eq!(
            fx.store.todo_get(p, id).unwrap().status,
            TodoStatus::InProgress
        );
        // Idempotent on a todo already in progress.
        fx.store.todo_start(p, id).unwrap();
        assert_eq!(
            fx.store.todo_get(p, id).unwrap().status,
            TodoStatus::InProgress
        );

        // Backlog/draft start cleanly too.
        let backlog = fx.store.todo_create(p, "later".into()).unwrap();
        fx.store
            .todo_update(
                p,
                backlog,
                TodoPatch {
                    status: Some(TodoStatus::Backlog),
                    ..Default::default()
                },
            )
            .unwrap();
        fx.store.todo_start(p, backlog).unwrap();
        assert_eq!(
            fx.store.todo_get(p, backlog).unwrap().status,
            TodoStatus::InProgress
        );

        // Terminal states refuse to be silently reopened.
        fx.store.todo_complete(p, id).unwrap();
        let err = fx.store.todo_start(p, id).unwrap_err();
        assert!(matches!(err, CoreError::BadRequest(_)));
        assert_eq!(
            fx.store.todo_get(p, id).unwrap().status,
            TodoStatus::Completed
        );

        assert!(matches!(
            fx.store.todo_start(p, 9_999),
            Err(CoreError::TodoNotFound(_))
        ));
    }

    #[test]
    fn todo_update_writes_only_the_some_fields() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "draft".into()).unwrap();
        fx.store
            .todo_update(
                p,
                id,
                TodoPatch {
                    body: Some("the description".into()),
                    priority: Some(Priority::High),
                    tags: Some(vec!["a".into(), "b".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        let t = fx.store.todo_get(p, id).unwrap();
        assert_eq!(t.title, "draft"); // None field left untouched
        assert_eq!(t.body, "the description");
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn completed_at_tracks_status_through_updates() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        assert!(fx.store.todo_get(p, id).unwrap().completed_at.is_none());

        let to = |s| TodoPatch {
            status: Some(s),
            ..Default::default()
        };
        fx.store
            .todo_update(p, id, to(TodoStatus::Completed))
            .unwrap();
        assert!(fx.store.todo_get(p, id).unwrap().completed_at.is_some());
        fx.store
            .todo_update(p, id, to(TodoStatus::InProgress))
            .unwrap();
        assert!(fx.store.todo_get(p, id).unwrap().completed_at.is_none());
    }

    #[test]
    fn blockers_record_list_and_reject_self_reference() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();

        fx.store.todo_add_blocker(p, b, a).unwrap();
        assert_eq!(fx.store.todo_get(p, b).unwrap().blockers, vec![a]);
        // Re-adding is idempotent; removing clears it.
        fx.store.todo_add_blocker(p, b, a).unwrap();
        assert_eq!(fx.store.todo_get(p, b).unwrap().blockers, vec![a]);
        fx.store.todo_remove_blocker(p, b, a).unwrap();
        assert!(fx.store.todo_get(p, b).unwrap().blockers.is_empty());

        assert!(matches!(
            fx.store.todo_add_blocker(p, a, a),
            Err(CoreError::BadRequest(_))
        ));
        assert!(matches!(
            fx.store.todo_add_blocker(p, a, 999),
            Err(CoreError::TodoNotFound(999))
        ));
    }

    #[test]
    fn deleting_a_todo_cascades_its_side_tables() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        fx.store.todo_add_blocker(p, b, a).unwrap();
        fx.store
            .todo_comment_add(p, a, "me".into(), "note".into())
            .unwrap();

        // Deleting a (the blocker) cascades away the (b blocked-by a) row.
        fx.store.todo_delete(p, a).unwrap();
        assert!(fx.store.todo_get(p, b).unwrap().blockers.is_empty());
        assert!(matches!(
            fx.store.todo_get(p, a),
            Err(CoreError::TodoNotFound(_))
        ));
    }

    #[test]
    fn comment_update_replaces_body_and_keeps_metadata() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        let cid = fx
            .store
            .todo_comment_add(p, id, "alice".into(), "first draft".into())
            .unwrap();
        let original = fx.store.todo_get(p, id).unwrap().comments[0].clone();

        fx.store
            .todo_comment_update(p, id, cid, "polished".into())
            .unwrap();
        let after = fx.store.todo_get(p, id).unwrap().comments[0].clone();
        assert_eq!(after.body, "polished");
        assert_eq!(after.author, original.author);
        assert_eq!(after.created_at, original.created_at);
    }

    #[test]
    fn comment_delete_removes_it_and_does_not_reuse_the_id() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        let c1 = fx
            .store
            .todo_comment_add(p, id, "a".into(), "1".into())
            .unwrap();
        let _c2 = fx
            .store
            .todo_comment_add(p, id, "a".into(), "2".into())
            .unwrap();

        fx.store.todo_comment_delete(p, id, c1).unwrap();
        let comments = fx.store.todo_get(p, id).unwrap().comments;
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "2");

        // The next add lands at 3, not 1: ids never recycle.
        let c3 = fx
            .store
            .todo_comment_add(p, id, "a".into(), "3".into())
            .unwrap();
        assert_eq!(c3, 3);
    }

    #[test]
    fn comment_update_and_delete_errors_on_missing_ids() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        // No such comment on an existing todo.
        assert!(matches!(
            fx.store.todo_comment_update(p, id, 999, "x".into()),
            Err(CoreError::TodoCommentNotFound { todo_id, comment_id })
                if todo_id == id && comment_id == 999
        ));
        // No such todo at all.
        assert!(matches!(
            fx.store.todo_comment_delete(p, 999, 1),
            Err(CoreError::TodoNotFound(999))
        ));
    }

    #[test]
    fn set_blockers_diffs_against_the_current_set() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        let c = fx.store.todo_create(p, "c".into()).unwrap();
        let target = fx.store.todo_create(p, "t".into()).unwrap();

        // From empty -> {a, b}.
        fx.store.todo_set_blockers(p, target, vec![a, b]).unwrap();
        assert_eq!(fx.store.todo_get(p, target).unwrap().blockers, vec![a, b]);

        // From {a, b} -> {b, c}: a removed, c added.
        fx.store.todo_set_blockers(p, target, vec![b, c]).unwrap();
        assert_eq!(fx.store.todo_get(p, target).unwrap().blockers, vec![b, c]);

        // Empty clears.
        fx.store.todo_set_blockers(p, target, vec![]).unwrap();
        assert!(fx.store.todo_get(p, target).unwrap().blockers.is_empty());

        // Self-blocking is rejected; a missing blocker errors before any write.
        assert!(matches!(
            fx.store.todo_set_blockers(p, target, vec![target]),
            Err(CoreError::BadRequest(_))
        ));
        assert!(matches!(
            fx.store.todo_set_blockers(p, target, vec![a, 999]),
            Err(CoreError::TodoNotFound(999))
        ));
        assert!(fx.store.todo_get(p, target).unwrap().blockers.is_empty());
    }

    #[test]
    fn tags_list_unions_and_sorts_across_todos() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        fx.store
            .todo_update(
                p,
                a,
                TodoPatch {
                    tags: Some(vec!["zeta".into(), "alpha".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        fx.store
            .todo_update(
                p,
                b,
                TodoPatch {
                    tags: Some(vec!["beta".into(), "alpha".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            fx.store.todo_tags_list(p).unwrap(),
            vec!["alpha", "beta", "zeta"]
        );
    }

    #[test]
    fn tags_list_unions_todos_and_notes() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let t = fx.store.todo_create(p, "t".into()).unwrap();
        let s = fx.store.note_create(p, "s".into()).unwrap();
        fx.store
            .todo_update(
                p,
                t,
                TodoPatch {
                    tags: Some(vec!["x".into(), "y".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        fx.store
            .note_update(
                p,
                s,
                NotePatch {
                    tags: Some(vec!["y".into(), "z".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        // Identical via both the unified core method and the back-compat alias,
        // and identical to the future `note_tags_list` MCP tool's source.
        assert_eq!(fx.store.tags_list(p).unwrap(), vec!["x", "y", "z"]);
        assert_eq!(fx.store.todo_tags_list(p).unwrap(), vec!["x", "y", "z"]);
    }

    #[test]
    fn note_update_round_trips_tags() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.note_create(p, "n".into()).unwrap();
        // Fresh pads start with empty tags (V8 column default).
        assert!(fx.store.note_get(p, id).unwrap().tags.is_empty());

        fx.store
            .note_update(
                p,
                id,
                NotePatch {
                    tags: Some(vec!["foo".into(), "bar".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            fx.store.note_get(p, id).unwrap().tags,
            vec!["foo".to_string(), "bar".to_string()]
        );

        // Tags land in the per-pad projection frontmatter.
        let pad_file = std::fs::read_to_string(root.join(".panopt/note/1.md")).unwrap();
        assert!(
            pad_file.contains("\ntags: foo, bar\n"),
            "projection carries `tags: foo, bar`:\n{pad_file}"
        );

        // An empty list clears the tags.
        fx.store
            .note_update(
                p,
                id,
                NotePatch {
                    tags: Some(vec![]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(fx.store.note_get(p, id).unwrap().tags.is_empty());
    }

    #[test]
    fn comment_ids_restart_in_each_todo() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        assert_eq!(
            fx.store
                .todo_comment_add(p, a, "x".into(), "1".into())
                .unwrap(),
            1
        );
        assert_eq!(
            fx.store
                .todo_comment_add(p, a, "x".into(), "2".into())
                .unwrap(),
            2
        );
        assert_eq!(
            fx.store
                .todo_comment_add(p, b, "y".into(), "1".into())
                .unwrap(),
            1
        );
        let comments = fx.store.todo_get(p, a).unwrap().comments;
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].body, "1");
        assert_eq!(comments[1].id, 2);
    }

    #[test]
    fn missing_ids_error() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        assert!(matches!(
            fx.store.todo_complete(p, 999),
            Err(CoreError::TodoNotFound(999))
        ));
        assert!(matches!(
            fx.store.note_append(p, 999, "x"),
            Err(CoreError::NoteNotFound(999))
        ));
        assert!(matches!(
            fx.store.note_read(p, 999),
            Err(CoreError::NoteNotFound(999))
        ));
    }

    #[test]
    fn todo_list_is_id_ascending() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        for t in ["a", "b", "c"] {
            fx.store.todo_create(p, t.into()).unwrap();
        }
        let ids: Vec<u64> = fx
            .store
            .todo_list(p)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn projects_are_isolated() {
        let mut fx = Fixture::new();
        let (a, _) = fx.project("alpha");
        let (b, _) = fx.project("beta");

        // Ids restart at 1 in each project.
        assert_eq!(fx.store.todo_create(a, "alpha task".into()).unwrap(), 1);
        assert_eq!(fx.store.todo_create(b, "beta task".into()).unwrap(), 1);

        let alpha = fx.store.todo_list(a).unwrap();
        let beta = fx.store.todo_list(b).unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(beta.len(), 1);
        assert_eq!(alpha[0].title, "alpha task");
        assert_eq!(beta[0].title, "beta task");
    }

    #[test]
    fn mutations_project_to_disk() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        let tid = fx.store.todo_create(p, "wire up auth".into()).unwrap();
        let index = std::fs::read_to_string(root.join(".panopt/todos.md")).unwrap();
        assert!(
            index.contains("- [ ] [#1](todos/1.md) wire up auth"),
            "{index}"
        );
        let todo_md = std::fs::read_to_string(root.join(".panopt/todos/1.md")).unwrap();
        assert!(todo_md.contains("status: open"), "{todo_md}");
        assert!(todo_md.contains("# wire up auth"), "{todo_md}");

        fx.store.todo_complete(p, tid).unwrap();
        let index = std::fs::read_to_string(root.join(".panopt/todos.md")).unwrap();
        assert!(
            index.contains("- [x] [#1](todos/1.md) wire up auth"),
            "{index}"
        );
        let todo_md = std::fs::read_to_string(root.join(".panopt/todos/1.md")).unwrap();
        assert!(todo_md.contains("status: completed"), "{todo_md}");

        let sid = fx.store.note_create(p, "notes".into()).unwrap();
        fx.store.note_append(p, sid, "first").unwrap();
        fx.store.note_append(p, sid, "second").unwrap();
        let sp_md = std::fs::read_to_string(root.join(format!(".panopt/note/{sid}.md"))).unwrap();
        // Per-pad files now carry a `created`/`updated` frontmatter block;
        // the wall-clock timestamps inside are checked structurally rather
        // than by exact match.
        assert!(sp_md.starts_with("---\n"), "{sp_md}");
        assert!(sp_md.contains("created: "), "{sp_md}");
        assert!(sp_md.contains("updated: "), "{sp_md}");
        assert!(sp_md.contains("# notes\n\nfirst\nsecond\n"), "{sp_md}");
    }

    #[test]
    fn state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("panopt.db");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        {
            let mut store = Store::open(&db).unwrap();
            let p = store.ensure_project(&root).unwrap();
            store.todo_create(p, "persist me".into()).unwrap();
            store.note_create(p, "kept".into()).unwrap();
        }
        {
            let mut store = Store::open(&db).unwrap();
            let p = store.ensure_project(&root).unwrap();
            let todos = store.todo_list(p).unwrap();
            assert_eq!(todos.len(), 1);
            assert_eq!(todos[0].title, "persist me");
            // The shared id counter resumes past the persisted todo (id 1)
            // and persisted note (id 2), so the next id is 3.
            assert_eq!(store.todo_create(p, "another".into()).unwrap(), 3);
        }
    }

    #[test]
    fn agents_register_identify_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        // A fresh project starts with an empty roster on disk.
        let agents_md = std::fs::read_to_string(root.join(".panopt/agents.md")).unwrap();
        assert!(agents_md.contains("_(no agents connected)_"), "{agents_md}");

        fx.store
            .agent_touch(p, "sess-1", KeySource::Session)
            .unwrap();
        fx.store
            .agent_identify(p, "sess-1", "backend".into(), Some("coding".into()))
            .unwrap();

        let me = fx.store.agent_whoami(p, "sess-1").unwrap();
        assert_eq!(me.name, "backend");
        assert_eq!(me.status, "coding");
        assert_eq!(fx.store.agent_list(p).unwrap().len(), 1);

        let agents_md = std::fs::read_to_string(root.join(".panopt/agents.md")).unwrap();
        assert!(
            agents_md.contains("- backend - coding (idle 0m)"),
            "{agents_md}"
        );

        // Backdate `last_seen` so the next projection shows ticking idle time.
        fx.store
            .test_backdate_last_seen(p, "sess-1", Duration::from_secs(900));
        fx.store
            .agent_identify(p, "sess-1", "backend".into(), Some("idle".into()))
            .unwrap();
        let agents_md = std::fs::read_to_string(root.join(".panopt/agents.md")).unwrap();
        assert!(
            agents_md.contains("- backend - idle (idle 15m)"),
            "{agents_md}"
        );
    }

    #[test]
    fn agent_rosters_are_project_isolated() {
        let mut fx = Fixture::new();
        let (a, _) = fx.project("alpha");
        let (b, _) = fx.project("beta");

        fx.store.agent_touch(a, "s1", KeySource::Session).unwrap();
        fx.store.agent_touch(b, "s2", KeySource::Session).unwrap();
        fx.store.agent_touch(b, "s3", KeySource::Session).unwrap();

        assert_eq!(fx.store.agent_list(a).unwrap().len(), 1);
        assert_eq!(fx.store.agent_list(b).unwrap().len(), 2);
        assert!(fx.store.agent_whoami(a, "s2").is_none());
    }

    #[test]
    fn sweep_keeps_fresh_agents() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        fx.store
            .agent_touch(p, "fresh", KeySource::Session)
            .unwrap();
        // Nothing is stale, so the sweep removes nothing.
        assert!(fx.store.sweep_idle_agents().unwrap().is_empty());
        assert_eq!(fx.store.agent_list(p).unwrap().len(), 1);
    }

    #[test]
    fn agent_leave_removes_entry_releases_locks_and_reprojects() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        fx.store
            .agent_touch(p, "stable", KeySource::Declared)
            .unwrap();
        fx.store
            .agent_identify(p, "stable", "greg-main".into(), None)
            .unwrap();
        // A lock the agent will leave behind.
        fx.store
            .lock_acquire(p, "stable", "auth".into(), None)
            .unwrap();
        assert_eq!(fx.store.lock_list(p).len(), 1);

        assert!(fx.store.agent_leave(p, "stable").unwrap());

        assert!(fx.store.agent_list(p).unwrap().is_empty());
        assert!(fx.store.lock_list(p).is_empty());

        // Projection mirrors the departure.
        let agents_md = std::fs::read_to_string(root.join(".panopt/agents.md")).unwrap();
        assert!(agents_md.contains("_(no agents connected)_"), "{agents_md}");
        let locks_md = std::fs::read_to_string(root.join(".panopt/locks.md")).unwrap();
        assert!(locks_md.contains("_(no locks held)_"), "{locks_md}");

        // Idempotent: leaving again is a no-op.
        assert!(!fx.store.agent_leave(p, "stable").unwrap());
    }

    #[test]
    fn sweep_keeps_declared_agents_even_when_idle() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        fx.store
            .agent_touch(p, "stable", KeySource::Declared)
            .unwrap();
        // Backdate `last_seen` past AGENT_MAX_IDLE so a session-keyed peer
        // would have been pruned. The declared entry must still survive.
        fx.store
            .test_backdate_last_seen(p, "stable", AGENT_MAX_IDLE * 10);
        assert!(fx.store.sweep_idle_agents().unwrap().is_empty());
        assert_eq!(fx.store.agent_list(p).unwrap().len(), 1);
    }

    #[test]
    fn empty_locks_md_after_bootstrap() {
        let mut fx = Fixture::new();
        let (_p, root) = fx.project("proj");
        let locks_md = std::fs::read_to_string(root.join(".panopt/locks.md")).unwrap();
        assert!(locks_md.contains("_(no locks held)_"), "{locks_md}");
    }

    #[test]
    fn locks_acquire_release_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        fx.store.agent_touch(p, "a", KeySource::Session).unwrap();
        fx.store
            .agent_identify(p, "a", "backend".into(), None)
            .unwrap();
        fx.store.agent_touch(p, "b", KeySource::Session).unwrap();

        // `a` acquires; `b` is denied and sees the holder's resolved name.
        assert_eq!(
            fx.store
                .lock_acquire(p, "a", "auth".into(), Some("token work".into()))
                .unwrap(),
            None
        );
        assert_eq!(
            fx.store.lock_acquire(p, "b", "auth".into(), None).unwrap(),
            Some("backend".to_string())
        );

        let locks_md = std::fs::read_to_string(root.join(".panopt/locks.md")).unwrap();
        assert!(
            locks_md.contains("- `auth` - held by backend - token work"),
            "{locks_md}"
        );

        // `a` releases; `b` can then take it.
        assert_eq!(fx.store.lock_release(p, "a", "auth").unwrap(), None);
        assert_eq!(
            fx.store.lock_acquire(p, "b", "auth".into(), None).unwrap(),
            None
        );

        let locks = fx.store.lock_list(p);
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].holder_key, "b");
    }

    #[test]
    fn locks_are_project_isolated() {
        let mut fx = Fixture::new();
        let (a, _) = fx.project("alpha");
        let (b, _) = fx.project("beta");

        assert_eq!(
            fx.store.lock_acquire(a, "x", "build".into(), None).unwrap(),
            None
        );
        // The same name in another project is unaffected.
        assert_eq!(
            fx.store.lock_acquire(b, "y", "build".into(), None).unwrap(),
            None
        );
        assert_eq!(fx.store.lock_list(a).len(), 1);
        assert_eq!(fx.store.lock_list(b).len(), 1);
    }

    #[test]
    fn agent_tool_create_list_update_delete_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        let id = fx
            .store
            .agent_tool_create(
                p,
                "claude".into(),
                "Mediator".into(),
                "claude --model sonnet".into(),
                String::new(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        assert_eq!(id, 1);

        let entries = fx.store.agent_tool_list(p).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "claude");
        assert_eq!(entries[0].display_name, "Mediator");
        assert!(entries[0].enabled);

        // The tool projection is what the cockpit reads for the spawn picker.
        let tools_md = std::fs::read_to_string(root.join(".panopt/agent_tools.md")).unwrap();
        assert!(tools_md.contains("- #1 Mediator"), "{tools_md}");

        fx.store
            .agent_tool_update(
                p,
                id,
                AgentToolPatch {
                    command: Some("claude".into()),
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        let updated = fx.store.agent_tool_get(p, id).unwrap();
        assert_eq!(updated.command, "claude");
        assert!(!updated.enabled);

        fx.store.agent_tool_delete(p, id).unwrap();
        assert!(fx.store.agent_tool_list(p).unwrap().is_empty());
        assert!(matches!(
            fx.store.agent_tool_delete(p, id),
            Err(CoreError::AgentToolNotFound(_))
        ));
    }

    #[test]
    fn agent_tool_create_validates_tool_type_against_the_registry() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");

        // A known profile key is accepted.
        assert!(fx
            .store
            .agent_tool_create(
                p,
                "ok".into(),
                String::new(),
                String::new(),
                String::new(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .is_ok());

        // An unknown key is rejected with a typed, caller-fixable error.
        let err = fx
            .store
            .agent_tool_create(
                p,
                "bad".into(),
                String::new(),
                String::new(),
                String::new(),
                "no-such-type".into(),
                String::new(),
                true,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::UnknownToolType(t) if t == "no-such-type"));
    }

    #[test]
    fn agent_tool_update_validates_a_changed_tool_type() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        let id = fx
            .store
            .agent_tool_create(
                p,
                "t".into(),
                String::new(),
                String::new(),
                String::new(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();

        // Retyping to an unknown profile is rejected...
        let err = fx
            .store
            .agent_tool_update(
                p,
                id,
                AgentToolPatch {
                    tool_type: Some("ghost".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::UnknownToolType(_)));

        // ...while a known retype, and a patch that leaves tool_type alone, both
        // succeed.
        assert!(fx
            .store
            .agent_tool_update(
                p,
                id,
                AgentToolPatch {
                    tool_type: Some("claude-code".into()),
                    ..Default::default()
                },
            )
            .is_ok());
        assert!(fx
            .store
            .agent_tool_update(
                p,
                id,
                AgentToolPatch {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .is_ok());
    }

    #[test]
    fn process_create_list_update_delete_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        let id = fx
            .store
            .process_create(
                p,
                ProcessKind::Command,
                "build".into(),
                "Build".into(),
                "cargo build".into(),
                "/tmp".into(),
                None,
            )
            .unwrap();
        assert_eq!(id, 1);

        let entries = fx.store.process_list(p).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, ProcessKind::Command);
        assert_eq!(entries[0].display_name, "Build");
        assert!(entries[0].agent_tool_id.is_none());

        let processes_md = std::fs::read_to_string(root.join(".panopt/processes.md")).unwrap();
        assert!(
            processes_md.contains("- [command] #1 Build"),
            "{processes_md}"
        );

        fx.store
            .process_update(
                p,
                id,
                ProcessPatch {
                    command: Some("cargo test".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fx.store.process_get(p, id).unwrap().command, "cargo test");

        fx.store.process_delete(p, id).unwrap();
        assert!(fx.store.process_list(p).unwrap().is_empty());
        assert!(matches!(
            fx.store.process_delete(p, id),
            Err(CoreError::ProcessNotFound(_))
        ));
    }

    #[test]
    fn process_start_report_stop_lifecycle() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();

        // start writes a copy-on-spawn `starting` row.
        let started = fx.store.process_start(p, config).unwrap();
        assert_eq!(started.kind, ProcessKind::Agent);
        assert_eq!(started.agent_tool_id, Some(config));
        assert_eq!(started.command, "claude");
        assert_eq!(started.cwd, "/work");
        // Identity is uniquified with the instance id (todo #190).
        assert_eq!(started.name, format!("claude-a-{}", started.id));
        assert_eq!(started.display_name, format!("Mediator #{}", started.id));
        assert_eq!(started.status.as_deref(), Some(process_status::STARTING));

        let md = std::fs::read_to_string(root.join(".panopt/processes.md")).unwrap();
        assert!(md.contains("· starting"), "{md}");

        // Pure factory: starting again spawns a second live instance, distinct
        // from the first (no focus-the-existing-one in the daemon).
        let again = fx.store.process_start(p, config).unwrap();
        assert_ne!(again.id, started.id);
        assert_eq!(fx.store.process_list(p).unwrap().len(), 2);

        // pid report flips to running and anchors liveness.
        fx.store
            .process_report(p, started.id, Some(4242), None, None)
            .unwrap();
        let row = fx.store.process_get(p, started.id).unwrap();
        assert_eq!(row.pid, Some(4242));
        assert_eq!(row.status.as_deref(), Some(process_status::RUNNING));
        assert!(row.last_seen.is_some());

        // pane-id report is independent and idempotent.
        fx.store
            .process_report(p, started.id, None, Some("17".into()), None)
            .unwrap();
        let row = fx.store.process_get(p, started.id).unwrap();
        assert_eq!(row.pane_id.as_deref(), Some("17"));
        assert_eq!(row.pid, Some(4242), "pane report must not clear the pid");

        // stop returns the pid to signal and leaves the row stopped.
        let killed = fx.store.process_stop(p, started.id).unwrap();
        assert_eq!(killed, Some(4242));
        let row = fx.store.process_get(p, started.id).unwrap();
        assert_eq!(row.status.as_deref(), Some(process_status::STOPPED));

        // each start is a fresh instance, stopped predecessors notwithstanding.
        let restarted = fx.store.process_start(p, config).unwrap();
        assert_ne!(restarted.id, started.id);
    }

    #[test]
    fn process_start_with_records_overrides_without_mutating_the_config() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();

        // A per-launch name + extra_args land on the instance row.
        let started = fx
            .store
            .process_start_with(
                p,
                config,
                Some("Mediator (run 1)".into()),
                vec!["--model".into(), "opus".into()],
            )
            .unwrap();
        // The per-launch override is the friendly base, suffixed with the id.
        assert_eq!(
            started.display_name,
            format!("Mediator (run 1) #{}", started.id)
        );
        assert_eq!(started.extra_args, vec!["--model", "opus"]);

        // The durable config is untouched: name and (absence of) args both stay.
        let cfg = fx.store.agent_tool_get(p, config).unwrap();
        assert_eq!(cfg.display_name, "Mediator");

        // Stop it, then a second launch with different overrides proves no
        // mutation carried over from the first.
        fx.store.process_stop(p, started.id).unwrap();
        let second = fx
            .store
            .process_start_with(p, config, None, vec!["--resume".into()])
            .unwrap();
        assert_ne!(second.id, started.id);
        // No name override falls back to the config's display name, suffixed.
        assert_eq!(second.display_name, format!("Mediator #{}", second.id));
        assert_eq!(second.extra_args, vec!["--resume"]);
        // And the first instance still carries its own args, unchanged.
        let first = fx.store.process_get(p, started.id).unwrap();
        assert_eq!(first.extra_args, vec!["--model", "opus"]);
    }

    #[test]
    fn process_start_is_a_factory_distinct_instances() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();

        // Two starts without stopping yield two distinct live instances of the
        // one config (the 1:N factory model, todo #190).
        let first = fx.store.process_start(p, config).unwrap();
        let second = fx.store.process_start(p, config).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(fx.store.process_list(p).unwrap().len(), 2);
        assert_eq!(first.agent_tool_id, Some(config));
        assert_eq!(second.agent_tool_id, Some(config));
        assert_eq!(
            first.status.as_deref(),
            Some(process_status::STARTING),
            "first stays live"
        );
        assert_eq!(second.status.as_deref(), Some(process_status::STARTING));
        // Identity is per-instance so the two never collide in the registry/locks.
        assert_ne!(first.name, second.name);
        assert_ne!(first.display_name, second.display_name);
        assert_eq!(first.name, format!("claude-a-{}", first.id));
        assert_eq!(second.name, format!("claude-a-{}", second.id));
    }

    #[test]
    fn process_start_empty_config_name_falls_back_to_agent_id() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        // A config with no name and no display name still yields a unique,
        // non-empty instance identity (so the edge never collides in the registry).
        let config = fx
            .store
            .agent_tool_create(
                p,
                String::new(),
                String::new(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();
        assert_eq!(started.name, format!("agent-{}", started.id));
        // A blank friendly base falls back to the internal name, never " #id".
        assert_eq!(started.display_name, format!("agent-{}", started.id));
    }

    #[test]
    fn process_start_with_name_override_is_suffixed() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        // Two spawns sharing one explicit override stay distinguishable: the id
        // suffix disambiguates them in the cockpit.
        let one = fx
            .store
            .process_start_with(p, config, Some("Reviewer".into()), Vec::new())
            .unwrap();
        let two = fx
            .store
            .process_start_with(p, config, Some("Reviewer".into()), Vec::new())
            .unwrap();
        assert_eq!(one.display_name, format!("Reviewer #{}", one.id));
        assert_eq!(two.display_name, format!("Reviewer #{}", two.id));
        assert_ne!(one.display_name, two.display_name);
    }

    #[test]
    fn process_output_and_search_read_the_capture_file() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();

        // No capture written yet -> empty, not an error.
        assert_eq!(fx.store.process_output(p, started.id, None).unwrap(), "");
        assert!(fx
            .store
            .search_output(p, started.id, "anything")
            .unwrap()
            .is_empty());

        // Simulate the cockpit teeing the pane viewport to the capture file.
        let dir = root.join(".panopt").join(".cockpit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("output-{}.txt", started.id)),
            "line one\nROUND 3 READY\nline three",
        )
        .unwrap();

        // Full read, tail-by-lines, and substring search.
        assert!(fx
            .store
            .process_output(p, started.id, None)
            .unwrap()
            .contains("ROUND 3 READY"));
        assert_eq!(
            fx.store.process_output(p, started.id, Some(1)).unwrap(),
            "line three"
        );
        assert_eq!(
            fx.store.search_output(p, started.id, "ROUND 3").unwrap(),
            vec!["ROUND 3 READY".to_string()]
        );

        // An unknown instance is rejected (the row must exist).
        assert!(fx.store.process_output(p, 9999, None).is_err());
    }

    #[test]
    fn render_agent_instructions_substitutes_real_process_facts() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "mediator-1".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();

        let rendered = fx
            .store
            .render_agent_instructions(
                p,
                &started,
                "127.0.0.1",
                7600,
                "secret-token",
                "/abs/panopt",
            )
            .unwrap()
            .expect("claude-code ships an instructions template");
        // The shipped template names the instance's real id and token, fully
        // substituted - no placeholders leak through.
        assert!(
            rendered.contains(&format!("process #{}", started.id)),
            "instructions name the process id:\n{rendered}"
        );
        assert!(rendered.contains("mediator-1"), "names the agent id");
        assert!(!rendered.contains("{{"), "no unrendered placeholder");
    }

    #[test]
    fn send_input_queues_projects_and_acks() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();

        // Two inputs queue in order and project to the cockpit JSONL file.
        fx.store.send_input(p, started.id, "first\n").unwrap();
        fx.store.send_input(p, started.id, "second\n").unwrap();
        let pending = fx.store.pending_inputs(p).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].content, "first\n");
        assert_eq!(pending[1].content, "second\n");
        assert!(pending[0].id < pending[1].id, "ordered by queue id");

        let inputs_file = root.join(".panopt/.cockpit/inputs.jsonl");
        let body = std::fs::read_to_string(&inputs_file).unwrap();
        assert_eq!(body.lines().count(), 2, "both rows projected:\n{body}");
        assert!(body.contains("\"content\":\"first\\n\""), "{body}");

        // Acking the first drops it from the queue and the projection; the
        // second remains.
        fx.store.ack_input(p, pending[0].id).unwrap();
        let remaining = fx.store.pending_inputs(p).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "second\n");
        let body = std::fs::read_to_string(&inputs_file).unwrap();
        assert_eq!(body.lines().count(), 1, "acked row gone:\n{body}");

        // Acking is idempotent - a repeat or unknown id is a no-op.
        fx.store.ack_input(p, pending[0].id).unwrap();
        assert_eq!(fx.store.pending_inputs(p).unwrap().len(), 1);
    }

    #[test]
    fn send_input_rejects_unknown_and_stopped_processes() {
        let mut fx = Fixture::new();
        let (p, _root) = fx.project("proj");
        // Unknown id.
        assert!(matches!(
            fx.store.send_input(p, 999, "x\n"),
            Err(CoreError::ProcessNotFound(_))
        ));
        // A stopped instance is not live, so input is refused rather than queued
        // for a process that will never read it.
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();
        fx.store.process_stop(p, started.id).unwrap();
        assert!(matches!(
            fx.store.send_input(p, started.id, "x\n"),
            Err(CoreError::BadRequest(_))
        ));
    }

    #[test]
    fn sweep_dead_processes_reaps_only_gone_pids() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();
        fx.store
            .process_report(p, started.id, Some(4242), None, None)
            .unwrap();

        // A `starting` row (no pid) of a second config must never be probed or
        // reaped - liveness only applies once a pid is anchored.
        let cfg2 = fx
            .store
            .agent_tool_create(
                p,
                "claude-b".into(),
                "Worker".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let starting = fx.store.process_start(p, cfg2).unwrap();

        // Predicate reports the pid alive: nothing is reaped.
        let reaped = fx.store.sweep_dead_processes(|_| true).unwrap();
        assert!(reaped.is_empty());
        assert_eq!(
            fx.store
                .process_get(p, started.id)
                .unwrap()
                .status
                .as_deref(),
            Some(process_status::RUNNING)
        );

        // Predicate reports the pid gone: the running row flips to exited and
        // its line leaves the projection; the pid-less `starting` row is intact.
        let reaped = fx.store.sweep_dead_processes(|_| false).unwrap();
        assert_eq!(reaped, vec![(p.0, started.id)]);
        let row = fx.store.process_get(p, started.id).unwrap();
        assert_eq!(row.status.as_deref(), Some(process_status::EXITED));
        assert_eq!(
            fx.store
                .process_get(p, starting.id)
                .unwrap()
                .status
                .as_deref(),
            Some(process_status::STARTING)
        );
        let md = std::fs::read_to_string(root.join(".panopt/processes.md")).unwrap();
        assert!(
            !md.contains("Mediator"),
            "reaped row still projected:\n{md}"
        );

        // A reaped (exited) row no longer blocks a fresh start of its config.
        let restarted = fx.store.process_start(p, config).unwrap();
        assert_ne!(restarted.id, started.id);
    }

    #[test]
    fn tick_process_projections_refreshes_idle_for_active_agents() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                "Mediator".into(),
                "claude".into(),
                "/work".into(),
                "claude-code".into(),
                String::new(),
                true,
            )
            .unwrap();
        let started = fx.store.process_start(p, config).unwrap();
        // Flip it to running, then report `idle` (which stamps `state_since`).
        fx.store
            .process_report(p, started.id, Some(4242), None, None)
            .unwrap();
        fx.store
            .process_report(p, started.id, None, None, Some("idle".into()))
            .unwrap();

        // Backdate `state_since` 180s so the next reproject renders a non-zero
        // whole-minute idle age. The reproject clock is `now`, so age >= 180s
        // -> `idle:3m`.
        fx.store
            .process_update(
                p,
                started.id,
                ProcessPatch {
                    state_since: Some(Some(now_epoch_secs() - 180)),
                    ..Default::default()
                },
            )
            .unwrap();

        let count = fx.store.tick_process_projections().unwrap();
        assert_eq!(count, 1, "one project has a live agent");
        let md = std::fs::read_to_string(root.join(".panopt/processes.md")).unwrap();
        assert!(md.contains("state:idle"), "state not idle:\n{md}");
        assert!(md.contains("idle:3m"), "idle age not refreshed:\n{md}");

        // A project with no live agent is skipped (no work, no panic).
        let (_q, _) = fx.project("empty");
        assert_eq!(fx.store.tick_process_projections().unwrap(), 1);
    }

    #[test]
    fn process_start_rejects_disabled_config() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let config = fx
            .store
            .agent_tool_create(
                p,
                "claude-a".into(),
                String::new(),
                "claude".into(),
                String::new(),
                "claude-code".into(),
                String::new(),
                false,
            )
            .unwrap();
        assert!(matches!(
            fx.store.process_start(p, config),
            Err(CoreError::BadRequest(_))
        ));
    }

    #[test]
    fn note_create_projects_the_index() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        fx.store.note_create(p, "design notes".into()).unwrap();
        fx.store.note_create(p, "scratch".into()).unwrap();
        let index = std::fs::read_to_string(root.join(".panopt/notes.md")).unwrap();
        assert!(index.contains("- [#1](note/1.md) design notes"), "{index}");
        assert!(index.contains("- [#2](note/2.md) scratch"), "{index}");
    }

    /// Soft delete keeps the row in SQLite but hides it from every live read
    /// path: list, get, the projection. The row is the seed for a future
    /// undelete surface; without it, recovery would have nothing to revive.
    #[test]
    fn soft_delete_keeps_the_row_but_hides_it_from_reads() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "going away".into()).unwrap();
        fx.store.todo_delete(p, id).unwrap();

        assert!(fx.store.todo_list(p).unwrap().is_empty());
        assert!(matches!(
            fx.store.todo_get(p, id).unwrap_err(),
            CoreError::TodoNotFound(_)
        ));
        // A second delete on the same id reports NotFound because the live
        // row is gone from the deleter's point of view.
        assert!(matches!(
            fx.store.todo_delete(p, id).unwrap_err(),
            CoreError::TodoNotFound(_)
        ));

        let surviving: i64 = fx
            .store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM todos WHERE project_id = ?1 AND deleted_at IS NOT NULL",
                [p.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(surviving, 1, "row stays behind for future undelete");
    }

    /// A blocker pointing at a soft-deleted todo must not surface in the
    /// blocked todo's `blockers` list - otherwise the form / projection
    /// shows a chip for a row the rest of the UI treats as gone.
    #[test]
    fn blocker_list_hides_soft_deleted_blockers() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let blocker = fx.store.todo_create(p, "upstream".into()).unwrap();
        let blocked = fx.store.todo_create(p, "downstream".into()).unwrap();
        fx.store.todo_add_blocker(p, blocked, blocker).unwrap();
        assert_eq!(
            fx.store.todo_get(p, blocked).unwrap().blockers,
            vec![blocker]
        );

        fx.store.todo_delete(p, blocker).unwrap();
        assert!(fx.store.todo_get(p, blocked).unwrap().blockers.is_empty());
    }

    /// Todo #122: a numeric query reaches the item by its `#N` id, even when no
    /// title/body text mentions that number. The `#`-prefixed form works too.
    #[test]
    fn note_search_matches_exact_id() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let _a = fx.store.note_create(p, "alpha".into()).unwrap();
        let target = fx.store.note_create(p, "beta".into()).unwrap();
        let _c = fx.store.note_create(p, "gamma".into()).unwrap();

        let by_num: Vec<u64> = fx
            .store
            .note_search(p, Some(&target.to_string()), &[])
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(by_num.contains(&target), "bare number finds #N: {by_num:?}");

        let by_hash: Vec<u64> = fx
            .store
            .note_search(p, Some(&format!("#{target}")), &[])
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            by_hash.contains(&target),
            "#-prefixed form finds #N: {by_hash:?}"
        );
    }

    /// The id predicate is an exact match, not a digit substring: searching `2`
    /// must not drag in `#20` (or `#12`), only `#2`. Titles carry no digits so
    /// the only matches come from the id clause.
    #[test]
    fn note_search_id_match_is_exact_not_substring() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        // ids 1..=20, all titled without digits so text matching can't fire.
        for _ in 0..20 {
            fx.store.note_create(p, "plain".into()).unwrap();
        }
        let ids: Vec<u64> = fx
            .store
            .note_search(p, Some("2"), &[])
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            ids,
            vec![2],
            "only the exact id #2, not #12 or #20: {ids:?}"
        );
    }

    /// The same exact-id reach applies to todos, alongside (not replacing) the
    /// existing text and status/priority filters.
    #[test]
    fn todo_search_matches_exact_id() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let _first = fx.store.todo_create(p, "groceries".into()).unwrap();
        let target = fx.store.todo_create(p, "taxes".into()).unwrap();

        let hits: Vec<u64> = fx
            .store
            .todo_search(p, Some(&target.to_string()), None, None, None, &[])
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            hits,
            vec![target],
            "numeric query finds exactly #N: {hits:?}"
        );
    }
}
