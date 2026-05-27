//! Plain data types for PANopt's shared state.
//!
//! These carry no serialization or transport concerns; `panoptd` maps them onto
//! its own wire shapes at the protocol boundary.

use std::time::SystemTime;

/// Opaque handle to a project in the [`crate::Store`].
///
/// Minted by [`crate::Store::ensure_project`] and passed back into store
/// methods to scope them to one project. Callers treat it as a token - they
/// never construct or inspect it - so the inner row id stays private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectId(pub(crate) i64);

/// A shared, append-oriented note identified by a stable numeric id.
///
/// The `id` (not the `title`) is the durable handle and the projected filename,
/// so renaming a scratchpad never moves its file. The id is unique within its
/// project and restarts at 1 in each project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scratchpad {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// Free-text labels drawn from the project's tag vocabulary, shared with
    /// todos (todo #61): [`crate::Store::tags_list`] returns the union across
    /// both kinds, so a tag set on one surface is offered up on the other.
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A set of optional edits to a [`Scratchpad`], applied by
/// [`crate::Store::scratchpad_update`].
///
/// Each `None` field leaves that attribute untouched; each `Some` replaces it.
#[derive(Debug, Default, Clone)]
pub struct ScratchpadPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// Lifecycle state of a [`Todo`].
///
/// The variants mirror Solo's `todos.status` column (DESIGN.md Section
/// 6.1) plus two panopt-specific additions, `Draft` and `NotDone`:
/// `Draft` is an early note the author has not yet committed to doing,
/// `Backlog` is not yet scheduled, `Open` is ready to pick up,
/// `InProgress` is being worked, `Completed` is done, and `NotDone`
/// records that the todo was closed without being done (cancelled,
/// won't-fix, superseded). Both `Completed` and `NotDone` are terminal;
/// only `Completed` sets `completed_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TodoStatus {
    #[default]
    Open,
    InProgress,
    Backlog,
    Draft,
    Completed,
    NotDone,
}

impl TodoStatus {
    /// The token stored in SQLite and used on the wire and in the projection.
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Open => "open",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Backlog => "backlog",
            TodoStatus::Draft => "draft",
            TodoStatus::Completed => "completed",
            TodoStatus::NotDone => "not_done",
        }
    }

    /// Parse a stored or wire token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<TodoStatus> {
        match s {
            "open" => Some(TodoStatus::Open),
            "in_progress" => Some(TodoStatus::InProgress),
            "backlog" => Some(TodoStatus::Backlog),
            "draft" => Some(TodoStatus::Draft),
            "completed" => Some(TodoStatus::Completed),
            "not_done" => Some(TodoStatus::NotDone),
            _ => None,
        }
    }
}

/// Importance of a [`Todo`], mirroring Solo's `todos.priority` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    High,
    #[default]
    Medium,
    Low,
}

impl Priority {
    /// The token stored in SQLite and used on the wire and in the projection.
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        }
    }

    /// Parse a stored or wire token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<Priority> {
        match s {
            "high" => Some(Priority::High),
            "medium" => Some(Priority::Medium),
            "low" => Some(Priority::Low),
            _ => None,
        }
    }
}

/// One comment on a [`Todo`], mirroring a row of Solo's `todo_comments` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoComment {
    /// Id unique within the parent todo, restarting at 1 in each todo.
    pub id: u64,
    /// Display name of the agent that posted it.
    pub author: String,
    pub body: String,
    /// SQLite `datetime('now')` text (UTC, `YYYY-MM-DD HH:MM:SS`).
    pub created_at: String,
}

/// A todo item identified by a numeric id, unique within its project.
///
/// The field set mirrors a trimmed subset of Solo's `todos` table plus its
/// `todo_comments` and `todo_blockers` side tables (DESIGN.md Section 6.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Todo {
    pub id: u64,
    pub title: String,
    /// Free-form description. Empty until set.
    pub body: String,
    pub status: TodoStatus,
    pub priority: Priority,
    /// Free-text owner; empty when unassigned. Unlike Solo's agent foreign key
    /// this is a plain name: PANopt's agent registry is in-memory and ephemeral
    /// (DESIGN.md Section 6.3), so a persisted agent id would dangle.
    pub assignee: String,
    pub tags: Vec<String>,
    /// Ids of other todos in the same project that block this one.
    pub blockers: Vec<u64>,
    pub comments: Vec<TodoComment>,
    /// SQLite `datetime('now')` text (UTC).
    pub created_at: String,
    pub updated_at: String,
    /// Set while `status` is `Completed`, `None` otherwise.
    pub completed_at: Option<String>,
}

/// A set of optional edits to a [`Todo`], applied by [`crate::Store::todo_update`].
///
/// Each `None` field leaves that attribute untouched; each `Some` replaces it.
#[derive(Debug, Default, Clone)]
pub struct TodoPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<TodoStatus>,
    pub priority: Option<Priority>,
    pub assignee: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// What a [`Process`] is - an agent CLI, a project command, or a plain
/// terminal. Modeled on the values of Solo's `processes.kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessKind {
    #[default]
    Agent,
    Command,
    Terminal,
}

impl ProcessKind {
    /// The token stored in SQLite and used on the wire and in the projection.
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessKind::Agent => "agent",
            ProcessKind::Command => "command",
            ProcessKind::Terminal => "terminal",
        }
    }

    /// Parse a stored or wire token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<ProcessKind> {
        match s {
            "agent" => Some(ProcessKind::Agent),
            "command" => Some(ProcessKind::Command),
            "terminal" => Some(ProcessKind::Terminal),
            _ => None,
        }
    }
}

/// A durable, per-project agent configuration: the "what to launch" half of
/// the two-layer process model (todo #27). One tool can back many running
/// [`Process`] instances. Modeled on a row of Solo's `agent_tools` table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentTool {
    /// Numeric id, unique within the project, drawn from the unified
    /// `projects.next_id` counter (todo #16).
    pub id: u64,
    /// Identifier-style name (e.g. "claude").
    pub name: String,
    /// Optional human label for the cockpit; falls back to `name` when empty.
    pub display_name: String,
    /// The shell command this tool launches when used to spawn a process.
    pub command: String,
    /// Working directory passed to the launched command; empty means the
    /// project root.
    pub cwd: String,
    /// Free-form tag for future categorization (Solo carries one per tool).
    /// Defaults to `"agent"`.
    pub tool_type: String,
    /// Whether the tool is offered in spawn UIs. Stored but not yet enforced
    /// (no spawn UI exists in PANopt yet).
    pub enabled: bool,
    /// Sort key within the project's agent tools.
    pub position: i64,
    /// SQLite `datetime('now')` text (UTC).
    pub created_at: String,
}

/// A set of optional edits to an [`AgentTool`], applied by
/// [`crate::Store::agent_tool_update`]. Each `None` field is left untouched.
#[derive(Debug, Default, Clone)]
pub struct AgentToolPatch {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub tool_type: Option<String>,
    pub enabled: Option<bool>,
    pub position: Option<i64>,
}

/// A per-project process instance: the "what's running" half of the
/// two-layer model (todo #27). Carries an optional back-reference to its
/// source [`AgentTool`] when `kind` is `Agent`, plus nullable lifecycle
/// columns reserved for a follow-up that wires spawn ownership.
///
/// Whether the process is currently alive is still *not* stored - the cockpit
/// continues to derive that from live Zellij pane state until panoptd owns
/// the spawn lifecycle (DESIGN.md S10).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Process {
    /// Numeric id, unique within the project, drawn from the unified
    /// `projects.next_id` counter.
    pub id: u64,
    pub kind: ProcessKind,
    /// Identifier-style name. Empty rows fall back to the linked tool's name
    /// at projection time.
    pub name: String,
    /// Optional human label for the cockpit; falls back to `name` when empty.
    pub display_name: String,
    /// The shell command this process executes. For agent kinds this is
    /// copied from the source [`AgentTool`] at spawn time so post-spawn edits
    /// to the tool don't perturb the running instance.
    pub command: String,
    /// Working directory; empty means the project root.
    pub cwd: String,
    /// Sort key within the project's processes.
    pub position: i64,
    /// Back-reference to the [`AgentTool`] this instance was spawned from.
    /// `None` for migrated pre-V6 rows and for command/terminal processes
    /// that have no backing tool. Deleting a tool sets this to `None`
    /// (ON DELETE SET NULL) so the live instance keeps running.
    pub agent_tool_id: Option<u64>,
    /// OS process id, populated only once panoptd owns the spawn lifecycle.
    pub pid: Option<i64>,
    /// Free-form lifecycle status (`"running"`, `"exited"`, ...). `None`
    /// until lifecycle ownership lands.
    pub status: Option<String>,
    /// Free-form agent state (`"idle"`, `"thinking"`, `"planning"`),
    /// produced by a future TUI-parsing layer.
    pub agent_state: Option<String>,
    /// SQLite `datetime('now')` text of the last lifecycle ping. `None`
    /// until lifecycle ownership lands.
    pub last_seen: Option<String>,
    /// SQLite `datetime('now')` text (UTC) at row creation.
    pub created_at: String,
}

/// A set of optional edits to a [`Process`], applied by
/// [`crate::Store::process_update`]. Each `None` field is left untouched.
#[derive(Debug, Default, Clone)]
pub struct ProcessPatch {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub position: Option<i64>,
    pub agent_tool_id: Option<Option<u64>>,
    pub pid: Option<Option<i64>>,
    pub status: Option<Option<String>>,
    pub agent_state: Option<Option<String>>,
    pub last_seen: Option<Option<String>>,
}

/// How the registry came to know about an agent.
///
/// The two key sources have very different lifetimes, and we treat them
/// differently for pruning (see [`crate::Store::sweep_idle_agents`]):
///
/// - `Declared` keys are stable `?agent=<id>` identifiers the launcher or the
///   user baked into the MCP URL. They name a *person or process*, not a
///   connection, and survive every kind of reconnect. Idle pruning would
///   silently delete a named agent during a quiet stretch, so declared keys
///   are kept until an explicit `agent_leave` (or daemon restart).
/// - `Session` keys are the rotating `mcp-session-id` header Claude Code mints
///   per connection. They really are throwaway - a single Claude Code agent
///   produces a stream of unrelated session keys over its lifetime - and the
///   idle sweep is what stops them from accumulating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Stable id from `?agent=<id>`. Survives idle pruning.
    Declared,
    /// Rotating `mcp-session-id` header. Pruned after `AGENT_MAX_IDLE`.
    Session,
}

impl KeySource {
    /// Lowercase wire form used in the MCP `agent_list` JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::Declared => "declared",
            KeySource::Session => "session",
        }
    }
}

/// A connected agent, as tracked by the in-memory registry.
///
/// Registry entries are never persisted, but their lifetimes differ by
/// [`KeySource`]: declared keys stay until explicit leave or daemon restart,
/// session keys age out via [`crate::Store::sweep_idle_agents`].
/// `first_seen` / `last_seen` drive both idle reporting and the pruning of
/// session-keyed agents.
#[derive(Debug, Clone)]
pub struct Agent {
    /// Opaque per-connection key (declared id or rotating MCP session id).
    /// Internal: it is the registry's map key, but is neither projected nor
    /// sent on the wire.
    pub key: String,
    /// Human-readable name. Defaults to the key until `identify` sets it.
    pub name: String,
    /// Free-form self-reported status, set via `identify`. Empty until then.
    pub status: String,
    /// Which kind of key the agent connected with. Stamped on first sight and
    /// never overwritten - a declared agent stays declared even if it later
    /// reconnects without `?agent=` (the new connection would register under
    /// a different session key, leaving the declared entry untouched).
    pub key_source: KeySource,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
}

/// An advisory lock: a named claim one agent holds to coordinate exclusive
/// work. The daemon never enforces it - agents cooperate voluntarily.
///
/// Like an [`Agent`], a lock is ephemeral and never persisted.
#[derive(Debug, Clone)]
pub struct Lock {
    /// What is locked - an arbitrary agreed-on name (a path, a task, a phase).
    pub name: String,
    /// Connection key of the holding agent.
    pub holder_key: String,
    /// The holder's display name, resolved from the registry when the lock is
    /// read or projected. Empty in stored entries - it is a read-time join.
    pub holder_name: String,
    /// Optional free-form reason the holder gave when acquiring.
    pub note: String,
    pub acquired_at: SystemTime,
}
