//! The single declaration of panopt's MCP tool surface.
//!
//! `panoptd` and `panopt _mcp-proxy` are both MCP servers that must publish
//! identical `tools/list` output. To prevent drift, this crate holds the one
//! source of truth: the `Parameters<T>` types whose `JsonSchema` derives
//! produce each tool's input schema (under [`params`]), and [`TOOL_SURFACE`]
//! which names every tool with its description and schema-generating
//! function.
//!
//! Both binaries iterate [`TOOL_SURFACE`] at startup to register their tool
//! routes - panoptd's routes call the real impls, the proxy's routes forward
//! to panoptd. There is no other declaration of the tool list anywhere;
//! adding a tool means adding one [`ToolDef`] here, one impl in panoptd's
//! dispatcher, and nothing in the proxy.

use schemars::generate::SchemaSettings;
use schemars::JsonSchema;

pub mod params;

// Re-export schemars so consumers don't pick up a second schemars version by
// accident. During the macro->data-driven migration (steps 2-4 of todo #88)
// panoptd's `#[tool]` macros need a `JsonSchema` impl that lines up with
// rmcp's pinned schemars; sharing this one re-export keeps the two in step.
pub use schemars;

use crate::params::*;

/// One MCP tool's publication metadata.
///
/// `schema_fn` rather than a baked schema so each entry can reference its
/// concrete `Parameters<T>` type without a const-eval dance: at registration
/// time the consumer calls the function pointer to get the schema for that
/// tool's arguments.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema_fn: fn() -> serde_json::Value,
}

/// Generate a tool's input schema from its argument type.
///
/// Matches `rmcp::handler::server::common::schema_for_type` byte-for-byte:
/// a draft2020-12 `SchemaSettings` generator, root-schema-for-`T`, serialized
/// to a JSON object. Used in [`TOOL_SURFACE`] entries as
/// `schema_fn: schema_for::<T>` so each entry is a plain function pointer.
/// Bit-identical output between this crate and rmcp matters so that the
/// step-4 drift assertion (and Claude's tool-list cache invalidation) treat
/// the two as truly the same schema.
pub fn schema_for<T: JsonSchema>() -> serde_json::Value {
    let generator = SchemaSettings::draft2020_12().into_generator();
    let schema = generator.into_root_schema_for::<T>();
    serde_json::to_value(schema).expect("a JsonSchema-derived schema always serializes to JSON")
}

/// Empty-object schema used for tools that take no arguments.
///
/// Hard-coded to match rmcp's `schema_for_empty_input` byte-for-byte. A
/// derived `#[derive(JsonSchema)] struct NoArgs {}` would add `$schema`,
/// `title`, and `description` fields that rmcp's empty-input shortcut omits.
pub fn schema_for_no_args() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// The publication table. Every MCP tool panoptd serves appears here exactly
/// once, in the order it was historically declared in `handler.rs`. Both
/// `panoptd` and `panopt _mcp-proxy` iterate this slice at startup to
/// register their tool routes.
pub const TOOL_SURFACE: &[ToolDef] = &[
    ToolDef {
        name: "identify",
        description: "Register or update this agent's name and status in the coordination \
                      registry. Other agents see it via agent_list.",
        schema_fn: schema_for::<IdentifyArgs>,
    },
    ToolDef {
        name: "agent_leave",
        description: "Cooperatively leave the project's agent registry. Removes this agent \
                      from agent_list immediately, releases every advisory lock it holds, \
                      and re-projects .panopt/agents.md and .panopt/locks.md. Idempotent: \
                      a second call by an already-gone agent is a silent ok. Intended for \
                      a clean handoff or shutdown - the idle sweep handles agents that \
                      just disappear.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "whoami",
        description: "Return this agent's own registry entry: {name, status, idle_seconds, \
                      is_self}.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "agent_list",
        description: "List every agent currently connected to this project as a JSON array \
                      of {name, status, idle_seconds, is_self}.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "lock_acquire",
        description: "Acquire a named advisory lock to coordinate exclusive work. \
                      Non-blocking: returns {acquired: bool, held_by?: name} - acquired \
                      is false when another agent holds it.",
        schema_fn: schema_for::<LockAcquireArgs>,
    },
    ToolDef {
        name: "lock_release",
        description: "Release a named advisory lock you hold. Returns {released: bool, \
                      held_by?: name}; released is false only if another agent holds it.",
        schema_fn: schema_for::<LockReleaseArgs>,
    },
    ToolDef {
        name: "lock_status",
        description: "List all advisory locks held in this project as a JSON array of \
                      {name, held_by, note, age_seconds, is_mine}.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "note_create",
        description: "Create a new note with a title. Returns its numeric id.",
        schema_fn: schema_for::<NoteCreateArgs>,
    },
    ToolDef {
        name: "note_list",
        description: "List all notes as a JSON array of {id, title}.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "note_search",
        description: "Find notes. Optional `query` substring-matches title and body \
                      (case-insensitive). Optional `tags` requires every listed tag to be \
                      present (AND semantics). Returns the same {id, title} shape as \
                      note_list; an empty-arg call is equivalent to note_list.",
        schema_fn: schema_for::<NoteSearchArgs>,
    },
    ToolDef {
        name: "note_append",
        description: "Append text to an existing note, addressed by numeric id.",
        schema_fn: schema_for::<NoteAppendArgs>,
    },
    ToolDef {
        name: "note_read",
        description: "Read the full body of a note, addressed by numeric id.",
        schema_fn: schema_for::<NoteReadArgs>,
    },
    ToolDef {
        name: "note_get",
        description: "Fetch one note in full - id, title, body, and timestamps - \
                      addressed by numeric id.",
        schema_fn: schema_for::<NoteGetArgs>,
    },
    ToolDef {
        name: "note_update",
        description: "Edit a note's title, body, and/or tags. Each omitted field is left \
                      unchanged; body replaces the existing body in full (use \
                      note_append to add instead of replace); tags replaces the whole \
                      tag list.",
        schema_fn: schema_for::<NoteUpdateArgs>,
    },
    ToolDef {
        name: "note_delete",
        description: "Delete a note. Removes both the database row and the per-note \
                      projection file.",
        schema_fn: schema_for::<NoteDeleteArgs>,
    },
    ToolDef {
        name: "note_tags_list",
        description: "List the project's tag vocabulary: the sorted, deduped union of every \
                      tag attached to any todo OR note. Identical output to \
                      todo_tags_list - the two surfaces share one project-wide tag pool.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "todo_create",
        description: "Create a new todo with a title. Returns its numeric id.",
        schema_fn: schema_for::<TodoCreateArgs>,
    },
    ToolDef {
        name: "todo_list",
        description: "List all todos as a JSON array of {id, title, status, priority, \
                      assignee, tags, blockers, comment_count, created_at, updated_at}. \
                      Use todo_get for a todo's body and comment thread.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "todo_search",
        description: "Find todos. Optional `query` substring-matches title and body \
                      (case-insensitive). Optional status/priority/assignee narrow by \
                      exact match (assignee is case-insensitive); pass an empty assignee \
                      string to find unassigned todos. Optional `tags` requires every \
                      listed tag (AND semantics). Returns the same shape as todo_list; \
                      an empty-arg call is equivalent to todo_list.",
        schema_fn: schema_for::<TodoSearchArgs>,
    },
    ToolDef {
        name: "todo_get",
        description: "Fetch one todo in full - body, comment thread, blockers and all - \
                      addressed by numeric id. `locked_by` is set when an agent holds \
                      the `todo:<id>` advisory lock.",
        schema_fn: schema_for::<TodoGetArgs>,
    },
    ToolDef {
        name: "todo_update",
        description: "Edit a todo's fields. Every argument but todo_id is optional; an \
                      omitted field is left unchanged. status is one of open/in_progress/\
                      backlog/draft/completed/not_done, priority one of high/medium/low; tags \
                      replaces the whole tag list.",
        schema_fn: schema_for::<TodoUpdateArgs>,
    },
    ToolDef {
        name: "todo_complete",
        description: "Mark a todo complete, addressed by numeric id.",
        schema_fn: schema_for::<TodoCompleteArgs>,
    },
    ToolDef {
        name: "todo_start",
        description: "Claim a todo for active work: atomically acquires the `todo:<id>` \
                      advisory lock, transitions status to `in_progress`, and returns the \
                      same full detail as `todo_get`. Use this as the first MCP call when \
                      you start work on a todo (e.g. the user asks 'do #N'). Idempotent \
                      when you already hold the lock and the todo is `in_progress`. If \
                      another agent holds the lock, returns {started: false, held_by} \
                      without mutating status. Errors on terminal states \
                      (`completed`/`not_done`) - reopen via `todo_update` first.",
        schema_fn: schema_for::<TodoStartArgs>,
    },
    ToolDef {
        name: "todo_delete",
        description: "Delete a todo, addressed by numeric id. Its comments and blocker \
                      links are removed with it.",
        schema_fn: schema_for::<TodoDeleteArgs>,
    },
    ToolDef {
        name: "todo_add_blocker",
        description: "Record that one todo is blocked by another: todo_id is blocked by \
                      blocker_id. Both must exist and must differ.",
        schema_fn: schema_for::<TodoBlockerArgs>,
    },
    ToolDef {
        name: "todo_remove_blocker",
        description: "Remove a blocker link, so todo_id is no longer blocked by \
                      blocker_id.",
        schema_fn: schema_for::<TodoBlockerArgs>,
    },
    ToolDef {
        name: "todo_comment_add",
        description: "Add a comment to a todo. The author is the supplied `author`, or - \
                      omitted - the calling agent's registered name. Returns the new \
                      comment's numeric id.",
        schema_fn: schema_for::<TodoCommentAddArgs>,
    },
    ToolDef {
        name: "todo_comment_update",
        description: "Edit an existing comment's body. The author and timestamp are \
                      preserved - this is not a re-post.",
        schema_fn: schema_for::<TodoCommentUpdateArgs>,
    },
    ToolDef {
        name: "todo_comment_delete",
        description: "Delete a comment from a todo. Comment ids are not reused after \
                      deletion - the per-todo counter keeps advancing.",
        schema_fn: schema_for::<TodoCommentDeleteArgs>,
    },
    ToolDef {
        name: "todo_set_blockers",
        description: "Replace a todo's blocker set in one call. Equivalent to a diff of \
                      todo_add_blocker / todo_remove_blocker, but atomic - used by the \
                      cockpit form to avoid a half-applied state.",
        schema_fn: schema_for::<TodoSetBlockersArgs>,
    },
    ToolDef {
        name: "todo_tags_list",
        description: "List the project's tag vocabulary: the sorted, deduped union of \
                      every tag attached to any todo OR note. The two surfaces \
                      share one project-wide tag pool, so this returns identical output \
                      to note_tags_list.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "todo_lock",
        description: "Claim a todo as `todo:<id>` in the advisory lock table. A thin \
                      wrapper around lock_acquire - non-blocking, returns {acquired: \
                      bool, held_by?: name}. Advisory only.",
        schema_fn: schema_for::<TodoLockArgs>,
    },
    ToolDef {
        name: "todo_unlock",
        description: "Release the `todo:<id>` advisory lock you hold. Returns \
                      {released: bool, held_by?: name}.",
        schema_fn: schema_for::<TodoUnlockArgs>,
    },
    ToolDef {
        name: "agent_tool_create",
        description: "Create an agent tool (config layer) - a durable agent configuration \
                      this project can spawn processes from. Returns its numeric id.",
        schema_fn: schema_for::<AgentToolCreateArgs>,
    },
    ToolDef {
        name: "agent_tool_list",
        description: "List this project's agent tools as a JSON array of {id, name, \
                      display_name, command, cwd, tool_type, enabled, position, created_at}.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "agent_tool_get",
        description: "Fetch one agent tool in full, addressed by numeric id.",
        schema_fn: schema_for::<AgentToolGetArgs>,
    },
    ToolDef {
        name: "agent_tool_update",
        description: "Edit an agent tool's fields. Every argument but agent_tool_id is \
                      optional; an omitted field is left unchanged.",
        schema_fn: schema_for::<AgentToolUpdateArgs>,
    },
    ToolDef {
        name: "agent_tool_delete",
        description: "Delete an agent tool, addressed by numeric id. Any processes that \
                      reference it keep running; their agent_tool_id back-reference is \
                      set to NULL.",
        schema_fn: schema_for::<AgentToolDeleteArgs>,
    },
    ToolDef {
        name: "process_create",
        description: "Create a process (instance layer) - a per-project agent/command/ \
                      terminal instance. Optionally links to a source agent_tool via \
                      agent_tool_id. Returns its numeric id.",
        schema_fn: schema_for::<ProcessCreateArgs>,
    },
    ToolDef {
        name: "process_list",
        description: "List this project's processes as a JSON array of {id, kind, name, \
                      display_name, command, cwd, position, agent_tool_id, pid, status, \
                      agent_state, last_seen, created_at}.",
        schema_fn: schema_for_no_args,
    },
    ToolDef {
        name: "process_get",
        description: "Fetch one process in full, addressed by numeric id.",
        schema_fn: schema_for::<ProcessGetArgs>,
    },
    ToolDef {
        name: "process_update",
        description: "Edit a process's fields. Every argument but process_id is \
                      optional; an omitted field is left unchanged.",
        schema_fn: schema_for::<ProcessUpdateArgs>,
    },
    ToolDef {
        name: "process_delete",
        description: "Delete a process, addressed by numeric id.",
        schema_fn: schema_for::<ProcessDeleteArgs>,
    },
    ToolDef {
        name: "id_kind",
        description: "Given a numeric id, return what kind of resource it is \
                      and a short label. Resolves across todos, notes, \
                      agent tools, and processes (ids are unified per project \
                      so a `#N` reference points to exactly one row). Errors \
                      `invalid_params` if the id matches no live (non-soft- \
                      deleted) resource.",
        schema_fn: schema_for::<IdKindArgs>,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tool_names_are_unique() {
        let mut seen = HashSet::new();
        for def in TOOL_SURFACE {
            assert!(
                seen.insert(def.name),
                "duplicate tool name in TOOL_SURFACE: {}",
                def.name
            );
        }
    }

    #[test]
    fn every_schema_serializes() {
        // The schema_fn pointers each call schemars::schema_for!(T) - smoke-test
        // that none of them panic or produce something serde_json can't render.
        for def in TOOL_SURFACE {
            let v = (def.schema_fn)();
            assert!(
                v.is_object(),
                "schema for {} is not a JSON object: {v}",
                def.name
            );
        }
    }
}
