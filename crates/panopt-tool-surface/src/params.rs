//! Request parameter structs for the MCP tools.
//!
//! Each derives `Deserialize` + `JsonSchema` so the surface table in
//! [`crate`] can generate each tool's input schema, and so panoptd can
//! deserialize incoming tool-call arguments. Doc comments on the fields
//! become the parameter descriptions agents see.
//!
//! `schemars` is re-exported from [`crate`] (and used directly here) at the
//! version pinned by this crate's Cargo.toml; both panoptd and the proxy
//! see the same schemars, so the schemas the proxy publishes are
//! bit-identical to the ones panoptd deserializes against.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};

/// Deserialize a field into a *double* `Option` so three input states stay
/// distinguishable: absent (`None`), present-and-`null` (`Some(None)`), and
/// present-with-a-value (`Some(Some(v))`). Paired with `#[serde(default)]`,
/// an omitted field is `None` while an explicit JSON `null` is `Some(None)`.
///
/// This exists to route around a defect in the MCP *client* (Claude Code's
/// tool-use → JSON serialization): when an argument value is the empty string
/// `""`, the client drops the entire arguments object before it leaves the
/// agent, so the daemon receives `{}` and rejects the call ("missing field
/// todo_id"). JSON `null` survives that path intact (verified end-to-end), so
/// fields that want a "clear this" signal accept `null` for it instead of `""`
/// — the one value the agent cannot actually transmit. See todo #239.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteCreateArgs {
    /// Human-readable title for the new note.
    pub title: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteAppendArgs {
    /// Numeric id of the note to append to.
    pub note_id: u64,
    /// Text to append. It is placed on its own line after existing content.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteReadArgs {
    /// Numeric id of the note to read.
    pub note_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteGetArgs {
    /// Numeric id of the note to fetch in full.
    pub note_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteUpdateArgs {
    /// Numeric id of the note to edit.
    pub note_id: u64,
    /// New title. Omit to leave unchanged.
    #[serde(default)]
    pub title: Option<String>,
    /// Replacement body. Replaces the existing body in full. Omit to leave
    /// unchanged.
    #[serde(default)]
    pub body: Option<String>,
    /// New complete tag list, replacing the old one. Tags share a project-wide
    /// vocabulary with todos (see `note_tags_list`). Omit to leave
    /// unchanged.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteDeleteArgs {
    /// Numeric id of the note to delete.
    pub note_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteSearchArgs {
    /// Case-insensitive substring matched against title and body. Omit to
    /// match every note (subject to other filters).
    #[serde(default)]
    pub query: Option<String>,
    /// Require every listed tag to be present on the note (AND
    /// semantics). Omit or pass an empty list to skip the tag filter.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoCreateArgs {
    /// Short description of the todo.
    pub title: String,
    /// Free-form description body. Omit for an empty body.
    #[serde(default)]
    pub body: Option<String>,
    /// Initial status: one of open, in_progress, backlog, draft, completed,
    /// not_done. Omit to default to open.
    #[serde(default)]
    pub status: Option<String>,
    /// Priority: one of high, medium, low. Omit to default to medium.
    #[serde(default)]
    pub priority: Option<String>,
    /// Assignee name. Omit to leave unassigned.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Initial tag list. Omit for no tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoCompleteArgs {
    /// Numeric id of the todo to mark complete.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoStartArgs {
    /// Numeric id of the todo to claim and transition to `in_progress`.
    pub todo_id: u64,
    /// Optional reason, forwarded to the `todo:<id>` advisory lock and shown
    /// to other agents in `lock_status`.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoGetArgs {
    /// Numeric id of the todo to fetch.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdKindArgs {
    /// Numeric id to resolve to its resource kind.
    pub id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoDeleteArgs {
    /// Numeric id of the todo to delete. Its comments and blocker links go too.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoUpdateArgs {
    /// Numeric id of the todo to edit.
    pub todo_id: u64,
    /// New title. Omit to leave unchanged.
    #[serde(default)]
    pub title: Option<String>,
    /// New free-form description body. Omit to leave unchanged.
    #[serde(default)]
    pub body: Option<String>,
    /// New status: one of open, in_progress, backlog, draft, completed,
    /// not_done. Omit to leave unchanged.
    #[serde(default)]
    pub status: Option<String>,
    /// New priority: one of high, medium, low. Omit to leave unchanged.
    #[serde(default)]
    pub priority: Option<String>,
    /// New assignee name. Pass JSON `null` to clear the assignee; omit to
    /// leave it unchanged. (Use `null`, not `""` — an empty-string argument is
    /// dropped in transit by the MCP client, taking the whole call with it;
    /// see todo #239.)
    #[serde(default, deserialize_with = "double_option")]
    pub assignee: Option<Option<String>>,
    /// New complete tag list, replacing the old one. Omit to leave unchanged.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoSearchArgs {
    /// Case-insensitive substring matched against title and body. Omit to
    /// match every todo (subject to other filters).
    #[serde(default)]
    pub query: Option<String>,
    /// Restrict to this status. One of open, in_progress, backlog, draft,
    /// completed, not_done. Omit to ignore status.
    #[serde(default)]
    pub status: Option<String>,
    /// Restrict to this priority. One of high, medium, low. Omit to ignore
    /// priority.
    #[serde(default)]
    pub priority: Option<String>,
    /// Case-insensitive exact match on assignee name. Pass an empty string
    /// to match only unassigned todos; omit to ignore assignee.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Require every listed tag to be present on the todo (AND semantics).
    /// Omit or pass an empty list to skip the tag filter.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoBlockerArgs {
    /// Numeric id of the blocked todo.
    pub todo_id: u64,
    /// Numeric id of the todo that blocks it.
    pub blocker_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoCommentAddArgs {
    /// Numeric id of the todo to comment on.
    pub todo_id: u64,
    /// Comment text.
    pub body: String,
    /// Author name to record. Omit to use the calling agent's registered name;
    /// a non-agent caller (the `panopt` CLI) supplies this explicitly.
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoCommentUpdateArgs {
    /// Numeric id of the todo the comment lives on.
    pub todo_id: u64,
    /// Numeric id of the comment to edit (per-todo, restarts at 1 in each todo).
    pub comment_id: u64,
    /// Replacement body. The author and timestamp are preserved.
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoCommentDeleteArgs {
    /// Numeric id of the todo the comment lives on.
    pub todo_id: u64,
    /// Numeric id of the comment to delete.
    pub comment_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoSetBlockersArgs {
    /// Numeric id of the todo whose blocker set is being replaced.
    pub todo_id: u64,
    /// Replacement set of blocker ids. May be empty to clear all blockers.
    pub blocker_ids: Vec<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoLockArgs {
    /// Numeric id of the todo to claim. The advisory lock name is `todo:<id>`.
    pub todo_id: u64,
    /// Optional reason, shown to other agents in `lock_status`.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoUnlockArgs {
    /// Numeric id of the todo whose lock to release.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentToolCreateArgs {
    /// Identifier-style name for the tool (e.g. "claude").
    pub name: String,
    /// Optional human label shown in the cockpit. Omit to use `name`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Shell command this tool launches when a process is spawned from it.
    #[serde(default)]
    pub command: Option<String>,
    /// Working directory passed to the launched command. Omit to use project root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Free-form tag for future categorization. Defaults to "agent".
    #[serde(default)]
    pub tool_type: Option<String>,
    /// Per-config system prompt the spawn template can land into a launch flag
    /// or file. Omit for an empty prompt (the agent's own default).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Whether the tool is offered in spawn UIs. Defaults to true.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentToolGetArgs {
    /// Numeric id of the agent tool to fetch.
    pub agent_tool_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentToolUpdateArgs {
    /// Numeric id of the agent tool to edit.
    pub agent_tool_id: u64,
    /// New name. Omit to leave unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// New display label. Omit to leave unchanged.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New launch command. Omit to leave unchanged.
    #[serde(default)]
    pub command: Option<String>,
    /// New working directory. Omit to leave unchanged.
    #[serde(default)]
    pub cwd: Option<String>,
    /// New tool_type tag. Omit to leave unchanged.
    #[serde(default)]
    pub tool_type: Option<String>,
    /// New system prompt. Omit to leave unchanged.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// New enabled flag. Omit to leave unchanged.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New sort position. Omit to leave unchanged.
    #[serde(default)]
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentToolDeleteArgs {
    /// Numeric id of the agent tool to delete.
    pub agent_tool_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessCreateArgs {
    /// Kind of process: one of agent, command, terminal.
    pub kind: String,
    /// Identifier-style name for the process.
    pub name: String,
    /// Optional human label shown in the cockpit. Omit to use `name`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Shell command the process executes. Omit for a bare terminal.
    #[serde(default)]
    pub command: Option<String>,
    /// Working directory for the process. Omit to use project root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Numeric id of the agent tool this process was spawned from. Omit for
    /// command and terminal processes that have no backing config.
    #[serde(default)]
    pub agent_tool_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessGetArgs {
    /// Numeric id of the process to fetch.
    pub process_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessUpdateArgs {
    /// Numeric id of the process to edit.
    pub process_id: u64,
    /// New name. Omit to leave unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// New display label. Omit to leave unchanged.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New launch command. Omit to leave unchanged.
    #[serde(default)]
    pub command: Option<String>,
    /// New working directory. Omit to leave unchanged.
    #[serde(default)]
    pub cwd: Option<String>,
    /// New sort position. Omit to leave unchanged.
    #[serde(default)]
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessDeleteArgs {
    /// Numeric id of the process to delete.
    pub process_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessStartArgs {
    /// Numeric id of the agent config (agent_tool) to start an instance of.
    pub agent_tool_id: u64,
    /// Per-launch display name for this instance. Overrides the config's name
    /// for this run only - the durable config is never touched. Omit to inherit
    /// the config's name.
    #[serde(default)]
    pub name: Option<String>,
    /// Per-launch extra arguments appended to the rendered spawn command for
    /// this instance only. Copied onto the instance, never written back to the
    /// config, so two launches of the same config can differ. Omit for none.
    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnAgentArgs {
    /// Id of an existing agent config to start an instance of. Omit for an
    /// ad-hoc spawn, in which case a fresh config is created from `tool_type`.
    #[serde(default)]
    pub agent_tool_id: Option<u64>,
    /// Agent type for an ad-hoc spawn (e.g. `claude-code`). Ignored when
    /// `agent_tool_id` is given. Defaults to the standard agent type.
    #[serde(default)]
    pub tool_type: Option<String>,
    /// Display name for the spawned agent. For an ad-hoc spawn this names the
    /// created config; for a config-backed spawn it overrides the instance's
    /// name for this run only.
    #[serde(default)]
    pub name: Option<String>,
    /// Opening task to hand the spawned agent: queued as its first input and
    /// typed into its pane once it is live. Omit to spawn an idle agent.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Per-launch extra arguments appended to the spawn command for this
    /// instance only, never written back to the config.
    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
    /// Opt-in idle auto-reap window, in seconds. When set, the daemon stops and
    /// deletes this instance once it has sat idle for at least this long - a
    /// safety valve for fire-and-forget sub-agents. Omit (the default) and the
    /// agent is never auto-killed: an idle-but-live agent you want to keep around
    /// is left running. Use for one-shot children you do not plan to reuse.
    #[serde(default)]
    pub idle_ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendInputArgs {
    /// Numeric id of the running instance (process) to type input into.
    pub process_id: u64,
    /// The text to write into the agent's pane. A trailing newline submits it,
    /// the same as if a human typed it.
    pub input: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InputAckArgs {
    /// Queue id (`seq`) of the delivered input, from the input projection.
    /// Cockpit-internal: the sidebar plugin calls this after writing the input
    /// into the pane, so the daemon drops it from the queue.
    pub seq: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForIdleArgs {
    /// Numeric ids of the running instances (processes) to wait on - typically
    /// the `process_id`s of agents you spawned.
    pub process_ids: Vec<u64>,
    /// How long to block before returning, in milliseconds. Capped server-side
    /// (a few minutes); if the cap or this value elapses first the call returns
    /// with `timed_out: true` and you can call again. Defaults to ~60s.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// "all" (default) returns once every listed process is idle or gone; "any"
    /// returns as soon as one is.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessOutputArgs {
    /// Numeric id of the instance (process) whose pane output to read.
    pub process_id: u64,
    /// How many of the most recent rendered terminal rows to return. Defaults to
    /// a recent window; the capture is bounded, so older scrollback is not kept.
    #[serde(default)]
    pub lines: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchOutputArgs {
    /// Numeric id of the instance (process) whose pane output to search.
    pub process_id: u64,
    /// Substring to look for in the captured pane output (e.g. a sentinel the
    /// child was told to print). Returns the matching rows.
    pub pattern: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessStopArgs {
    /// Numeric id of the process (instance) to stop.
    pub process_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProcessReportArgs {
    /// Numeric id of the process (instance) being reported on.
    pub process_id: u64,
    /// OS process id of the live instance. Setting it flips the row to
    /// `running`. Reported by the edge wrapper, whose pid survives its exec
    /// into the agent.
    #[serde(default)]
    pub pid: Option<i64>,
    /// Opaque identifier of the pane the instance landed in. Reported
    /// best-effort by the cockpit plugin. Omit to leave unchanged.
    #[serde(default)]
    pub pane_id: Option<String>,
    /// Latest derived agent activity state. Omit to leave unchanged.
    #[serde(default)]
    pub agent_state: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdentifyArgs {
    /// Human-readable name for this agent, shown to others in the registry.
    pub name: String,
    /// Optional free-form status, for example "implementing auth" or "blocked".
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LockAcquireArgs {
    /// Name of the advisory lock to acquire - an agreed-on string such as a
    /// path, a task, or a phase of work.
    pub name: String,
    /// Optional reason for holding the lock, shown to other agents.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LockReleaseArgs {
    /// Name of the advisory lock to release.
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};

    /// The reporter's hypothesis was a serde defect in `TodoUpdateArgs`. It
    /// isn't: a `{todo_id, status}` payload parses cleanly. The "missing field
    /// todo_id" they saw only arises from an *empty* arguments object, which is
    /// what the MCP client actually delivered after dropping the call (#239).
    #[test]
    fn todo_update_parses_minimal_payload() {
        let args: TodoUpdateArgs =
            from_value(json!({"todo_id": 16, "status": "open"})).expect("must parse");
        assert_eq!(args.todo_id, 16);
        assert_eq!(args.status.as_deref(), Some("open"));
        assert_eq!(args.assignee, None); // absent -> leave unchanged
    }

    /// The three `assignee` input states must stay distinguishable so the
    /// handler can tell "leave unchanged" from "clear". This is the whole point
    /// of the double `Option`.
    #[test]
    fn todo_update_assignee_tristate() {
        // absent -> None -> leave unchanged
        let omitted: TodoUpdateArgs = from_value(json!({"todo_id": 1})).unwrap();
        assert_eq!(omitted.assignee, None);

        // explicit null -> Some(None) -> clear
        let cleared: TodoUpdateArgs =
            from_value(json!({"todo_id": 1, "assignee": null})).unwrap();
        assert_eq!(cleared.assignee, Some(None));

        // a name -> Some(Some(name)) -> set
        let set: TodoUpdateArgs =
            from_value(json!({"todo_id": 1, "assignee": "greg"})).unwrap();
        assert_eq!(set.assignee, Some(Some("greg".to_string())));
    }

    /// An arguments object with no `todo_id` is the exact shape that reaches the
    /// daemon after the client drops an empty-string call - and the exact source
    /// of the reported error. Pin that this is the only thing that fails.
    #[test]
    fn todo_update_rejects_empty_object() {
        let err = from_value::<TodoUpdateArgs>(json!({})).unwrap_err();
        assert!(
            err.to_string().contains("todo_id"),
            "expected a missing-todo_id error, got: {err}"
        );
    }
}
