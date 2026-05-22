//! The MCP server handler: the coordination tools and the `ServerHandler`
//! impl that advertises them.
//!
//! Every tool resolves its project from the `ws` query parameter on the
//! request URL (DESIGN.md Section 5.3) and registers the calling agent by its
//! agent key - the `agent` URL parameter when set, the MCP session id
//! otherwise - so one daemon serves every project at once with no per-session
//! state welded into the handler.

use std::sync::{Arc, Mutex};

use http::request::Parts;
use panopt_core::{
    Agent, CoreError, Lock, Priority, ProjectId, Store, Todo, TodoPatch, TodoStatus,
};
use rmcp::{
    handler::server::{common::Extension, router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    ErrorData as McpError, ServerHandler,
};
use serde::Serialize;

use crate::params::{
    IdentifyArgs, LockAcquireArgs, LockReleaseArgs, ScratchpadAppendArgs, ScratchpadCreateArgs,
    ScratchpadReadArgs, TodoBlockerArgs, TodoCommentAddArgs, TodoCompleteArgs, TodoCreateArgs,
    TodoDeleteArgs, TodoGetArgs, TodoUpdateArgs,
};

/// Per-session MCP handler.
///
/// rmcp builds one of these per MCP session via the factory in `main`. Each
/// holds a *clone of the shared `Arc`*, so every session - and therefore every
/// connected agent - mutates and reads the one `Mutex<Store>`. The session
/// carries no project: each tool call derives it from the request URL.
#[derive(Clone)]
pub struct Handler {
    state: Arc<Mutex<Store>>,
    // Part of rmcp's `#[tool_router]` / `#[tool_handler]` pattern: built once in
    // `new()` and consulted by the macro-generated `ServerHandler` impl. The
    // dead-code lint does not attribute that macro-generated use to the field.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

/// Wire shape for a scratchpad in `scratchpad_list` output.
#[derive(Serialize)]
struct ScratchpadDto {
    id: u64,
    title: String,
}

/// Wire shape for a todo in `todo_list` output: every field but the body and
/// the full comment thread, which `todo_get` returns.
#[derive(Serialize)]
struct TodoSummaryDto {
    id: u64,
    title: String,
    status: &'static str,
    priority: &'static str,
    assignee: String,
    tags: Vec<String>,
    blockers: Vec<u64>,
    comment_count: usize,
}

impl TodoSummaryDto {
    fn from_todo(todo: &Todo) -> Self {
        TodoSummaryDto {
            id: todo.id,
            title: todo.title.clone(),
            status: todo.status.as_str(),
            priority: todo.priority.as_str(),
            assignee: todo.assignee.clone(),
            tags: todo.tags.clone(),
            blockers: todo.blockers.clone(),
            comment_count: todo.comments.len(),
        }
    }
}

/// Wire shape for one comment in `todo_get` output.
#[derive(Serialize)]
struct TodoCommentDto {
    id: u64,
    author: String,
    body: String,
    created_at: String,
}

/// Wire shape for the full todo returned by `todo_get`.
#[derive(Serialize)]
struct TodoDetailDto {
    id: u64,
    title: String,
    body: String,
    status: &'static str,
    priority: &'static str,
    assignee: String,
    tags: Vec<String>,
    blockers: Vec<u64>,
    comments: Vec<TodoCommentDto>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

impl TodoDetailDto {
    fn from_todo(todo: Todo) -> Self {
        TodoDetailDto {
            id: todo.id,
            title: todo.title,
            body: todo.body,
            status: todo.status.as_str(),
            priority: todo.priority.as_str(),
            assignee: todo.assignee,
            tags: todo.tags,
            blockers: todo.blockers,
            comments: todo
                .comments
                .into_iter()
                .map(|c| TodoCommentDto {
                    id: c.id,
                    author: c.author,
                    body: c.body,
                    created_at: c.created_at,
                })
                .collect(),
            created_at: todo.created_at,
            updated_at: todo.updated_at,
            completed_at: todo.completed_at,
        }
    }
}

/// Wire shape for an agent in `agent_list` and `whoami` output.
#[derive(Serialize)]
struct AgentDto {
    name: String,
    status: String,
    /// Seconds since this agent's last tool call.
    idle_seconds: u64,
    /// True for the agent that made this request.
    is_self: bool,
}

impl AgentDto {
    fn from_agent(agent: &Agent, is_self: bool) -> Self {
        let idle_seconds = std::time::SystemTime::now()
            .duration_since(agent.last_seen)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        AgentDto {
            name: agent.name.clone(),
            status: agent.status.clone(),
            idle_seconds,
            is_self,
        }
    }
}

/// Wire shape for an advisory lock in `lock_status` output.
#[derive(Serialize)]
struct LockDto {
    name: String,
    held_by: String,
    note: String,
    /// Seconds since the lock was acquired.
    age_seconds: u64,
    /// True if the agent that made this request holds the lock.
    is_mine: bool,
}

impl LockDto {
    fn from_lock(lock: &Lock, caller_key: Option<&str>) -> Self {
        let age_seconds = std::time::SystemTime::now()
            .duration_since(lock.acquired_at)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        LockDto {
            name: lock.name.clone(),
            held_by: lock.holder_name.clone(),
            note: lock.note.clone(),
            age_seconds,
            is_mine: caller_key == Some(lock.holder_key.as_str()),
        }
    }
}

/// Map a core error onto an MCP error result at the protocol boundary.
fn map_core_err(e: CoreError) -> McpError {
    match e {
        // Caller-fixable: a bad id, a rejected argument, or a workspace path
        // the daemon cannot reach.
        CoreError::ScratchpadNotFound(_)
        | CoreError::TodoNotFound(_)
        | CoreError::BadRequest(_)
        | CoreError::Workspace(_) => McpError::invalid_params(e.to_string(), None),
        // Internal: a stale project handle, a database fault, or a failed write.
        CoreError::ProjectNotFound(_) | CoreError::Db(_) | CoreError::Projection(_) => {
            McpError::internal_error(e.to_string(), None)
        }
    }
}

/// Find `key`'s value in a URL query string, percent-decoded.
///
/// `query` is the raw query component, with no leading `?`, as returned by
/// [`http::Uri::query`].
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let raw = query?
        .split('&')
        .find_map(|kv| kv.strip_prefix(prefix.as_str()))?;
    Some(
        percent_encoding::percent_decode_str(raw)
            .decode_utf8_lossy()
            .into_owned(),
    )
}

/// Resolve the project for a request from the `ws` query parameter on the MCP
/// server URL.
///
/// Agents register PANopt per project with `?ws=<project path>` appended to the
/// URL, so the daemon scopes every call without inferring anything. The path is
/// percent-decoded, so workspace paths containing spaces survive the round trip.
fn resolve_project(store: &mut Store, parts: &Parts) -> Result<ProjectId, McpError> {
    let path = query_param(parts.uri.query(), "ws").ok_or_else(|| {
        McpError::invalid_params(
            "no project: the MCP server URL must end with ?ws=<absolute project path>",
            None,
        )
    })?;
    store
        .ensure_project(std::path::Path::new(&path))
        .map_err(map_core_err)
}

/// The calling agent's stable key.
///
/// Prefers the `agent` query parameter on the MCP URL - a stable per-agent id
/// the launcher injects, which survives MCP session churn. Falls back to the
/// `mcp-session-id` header when no `agent` is set; that header rotates whenever
/// the client's connection drops and re-initializes, so without an `agent` id
/// one agent can briefly appear under several keys (see DESIGN.md Section 9).
/// `None` only for a request that carries neither.
fn agent_key(parts: &Parts) -> Option<String> {
    query_param(parts.uri.query(), "agent")
        .filter(|id| !id.is_empty())
        .or_else(|| {
            parts
                .headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
}

/// Resolve the request's project and register the calling agent in it.
///
/// Every tool runs this first, so any connected agent appears in the registry
/// even if it never calls `identify`. Returns the project and the agent key
/// (the latter `None` only when the request carries no MCP session id).
///
/// A request carrying `?observer=1` is a tool, not an agent - the `panopt`
/// CLI, say - so it resolves the project but is never added to the registry,
/// and its key is reported as `None`.
fn enter(store: &mut Store, parts: &Parts) -> Result<(ProjectId, Option<String>), McpError> {
    let project = resolve_project(store, parts)?;
    if query_param(parts.uri.query(), "observer").as_deref() == Some("1") {
        return Ok((project, None));
    }
    let key = agent_key(parts);
    if let Some(key) = &key {
        let first_seen = store.agent_whoami(project, key).is_none();
        store.agent_touch(project, key).map_err(map_core_err)?;
        if first_seen {
            let from_url = query_param(parts.uri.query(), "agent")
                .filter(|id| !id.is_empty())
                .is_some();
            tracing::info!(
                agent = %key,
                key_source = if from_url { "agent= URL parameter" } else { "MCP session id" },
                "agent connected"
            );
        }
    }
    Ok((project, key))
}

/// Require an agent key, failing with a clear message when the request carried
/// neither an `agent` parameter nor an MCP session id (the registry and lock
/// tools need one).
fn require_key(key: Option<String>) -> Result<String, McpError> {
    key.ok_or_else(|| {
        McpError::invalid_params(
            "this request has no agent identity (no ?agent= parameter and no MCP \
             session id); the registry and lock tools need one",
            None,
        )
    })
}

/// Reject an empty or whitespace-only lock name.
fn require_lock_name(name: String) -> Result<String, McpError> {
    if name.trim().is_empty() {
        return Err(McpError::invalid_params("lock name must not be empty", None));
    }
    Ok(name)
}

/// Parse a status token from a tool argument, with a caller-facing error.
fn parse_status(s: &str) -> Result<TodoStatus, McpError> {
    TodoStatus::parse(s).ok_or_else(|| {
        McpError::invalid_params(
            format!("invalid status '{s}': expected open, in_progress, backlog, or completed"),
            None,
        )
    })
}

/// Parse a priority token from a tool argument, with a caller-facing error.
fn parse_priority(s: &str) -> Result<Priority, McpError> {
    Priority::parse(s).ok_or_else(|| {
        McpError::invalid_params(
            format!("invalid priority '{s}': expected high, medium, or low"),
            None,
        )
    })
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json =
        serde_json::to_string(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

#[tool_router]
impl Handler {
    pub fn new(state: Arc<Mutex<Store>>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Register or update this agent's name and status in the coordination \
                          registry. Other agents see it via agent_list.")]
    async fn identify(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<IdentifyArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            let name = args.name;
            st.agent_identify(project, &key, name.clone(), args.status)
                .map_err(map_core_err)?;
            tracing::info!(agent = %key, %name, "agent identified");
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Return this agent's own registry entry: {name, status, idle_seconds, \
                          is_self}.")]
    async fn whoami(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            let agent = st
                .agent_whoami(project, &key)
                .ok_or_else(|| McpError::internal_error("caller not found in registry", None))?;
            AgentDto::from_agent(&agent, true)
        };
        json_result(&dto)
    }

    #[tool(description = "List every agent currently connected to this project as a JSON array \
                          of {name, status, idle_seconds, is_self}.")]
    async fn agent_list(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let dtos: Vec<AgentDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            st.agent_list(project)
                .map_err(map_core_err)?
                .iter()
                .map(|a| AgentDto::from_agent(a, Some(a.key.as_str()) == key.as_deref()))
                .collect()
        };
        json_result(&dtos)
    }

    #[tool(description = "Acquire a named advisory lock to coordinate exclusive work. \
                          Non-blocking: returns {acquired: bool, held_by?: name} - acquired \
                          is false when another agent holds it.")]
    async fn lock_acquire(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<LockAcquireArgs>,
    ) -> Result<CallToolResult, McpError> {
        let name = require_lock_name(args.name)?;
        let outcome = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            st.lock_acquire(project, &key, name, args.note)
                .map_err(map_core_err)?
        };
        match outcome {
            None => json_result(&serde_json::json!({ "acquired": true })),
            Some(holder) => json_result(&serde_json::json!({ "acquired": false, "held_by": holder })),
        }
    }

    #[tool(description = "Release a named advisory lock you hold. Returns {released: bool, \
                          held_by?: name}; released is false only if another agent holds it.")]
    async fn lock_release(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<LockReleaseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let name = require_lock_name(args.name)?;
        let outcome = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            st.lock_release(project, &key, &name).map_err(map_core_err)?
        };
        match outcome {
            None => json_result(&serde_json::json!({ "released": true })),
            Some(holder) => json_result(&serde_json::json!({ "released": false, "held_by": holder })),
        }
    }

    #[tool(description = "List all advisory locks held in this project as a JSON array of \
                          {name, held_by, note, age_seconds, is_mine}.")]
    async fn lock_status(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let dtos: Vec<LockDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            st.lock_list(project)
                .iter()
                .map(|l| LockDto::from_lock(l, key.as_deref()))
                .collect()
        };
        json_result(&dtos)
    }

    #[tool(description = "Create a new scratchpad with a title. Returns its numeric id.")]
    async fn scratchpad_create(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<ScratchpadCreateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_create(project, args.title).map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }

    #[tool(description = "List all scratchpads as a JSON array of {id, title}.")]
    async fn scratchpad_list(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let dtos: Vec<ScratchpadDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_list(project)
                .map_err(map_core_err)?
                .into_iter()
                .map(|(id, title)| ScratchpadDto { id, title })
                .collect()
        };
        json_result(&dtos)
    }

    #[tool(description = "Append text to an existing scratchpad, addressed by numeric id.")]
    async fn scratchpad_append(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<ScratchpadAppendArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_append(project, args.scratchpad_id, &args.content)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Read the full body of a scratchpad, addressed by numeric id.")]
    async fn scratchpad_read(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<ScratchpadReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_read(project, args.scratchpad_id)
                .map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(description = "Create a new todo with a title. Returns its numeric id.")]
    async fn todo_create(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoCreateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_create(project, args.title).map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }

    #[tool(description = "List all todos as a JSON array of {id, title, status, priority, \
                          assignee, tags, blockers, comment_count}. Use todo_get for a todo's \
                          body and comment thread.")]
    async fn todo_list(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let dtos: Vec<TodoSummaryDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_list(project)
                .map_err(map_core_err)?
                .iter()
                .map(TodoSummaryDto::from_todo)
                .collect()
        };
        json_result(&dtos)
    }

    #[tool(description = "Fetch one todo in full - body, comment thread, blockers and all - \
                          addressed by numeric id.")]
    async fn todo_get(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            TodoDetailDto::from_todo(st.todo_get(project, args.todo_id).map_err(map_core_err)?)
        };
        json_result(&dto)
    }

    #[tool(description = "Edit a todo's fields. Every argument but todo_id is optional; an \
                          omitted field is left unchanged. status is one of open/in_progress/\
                          backlog/completed, priority one of high/medium/low; tags replaces \
                          the whole tag list.")]
    async fn todo_update(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoUpdateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let status = match &args.status {
            Some(s) => Some(parse_status(s)?),
            None => None,
        };
        let priority = match &args.priority {
            Some(p) => Some(parse_priority(p)?),
            None => None,
        };
        let patch = TodoPatch {
            title: args.title,
            body: args.body,
            status,
            priority,
            assignee: args.assignee,
            tags: args.tags,
        };
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_update(project, args.todo_id, patch).map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Mark a todo complete, addressed by numeric id.")]
    async fn todo_complete(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoCompleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_complete(project, args.todo_id).map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Delete a todo, addressed by numeric id. Its comments and blocker \
                          links are removed with it.")]
    async fn todo_delete(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoDeleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_delete(project, args.todo_id).map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Record that one todo is blocked by another: todo_id is blocked by \
                          blocker_id. Both must exist and must differ.")]
    async fn todo_add_blocker(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoBlockerArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_add_blocker(project, args.todo_id, args.blocker_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Remove a blocker link, so todo_id is no longer blocked by \
                          blocker_id.")]
    async fn todo_remove_blocker(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoBlockerArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_remove_blocker(project, args.todo_id, args.blocker_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    #[tool(description = "Add a comment to a todo. The author is the supplied `author`, or - \
                          omitted - the calling agent's registered name. Returns the new \
                          comment's numeric id.")]
    async fn todo_comment_add(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(args): Parameters<TodoCommentAddArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            // An explicit author wins; otherwise the agent's registered name;
            // otherwise its raw key; "unknown" only for an anonymous observer.
            let author = args
                .author
                .filter(|a| !a.trim().is_empty())
                .or_else(|| {
                    key.as_deref()
                        .and_then(|k| st.agent_whoami(project, k))
                        .map(|a| a.name)
                })
                .or(key)
                .unwrap_or_else(|| "unknown".to_string());
            st.todo_comment_add(project, args.todo_id, author, args.body)
                .map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }
}

#[tool_handler]
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        // `Implementation::from_build_env()` reports rmcp's own package
        // metadata; override name/version so clients see "panoptd".
        let mut server_info = Implementation::from_build_env();
        server_info.name = "panoptd".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(
                "PANopt coordination daemon: shared todos and scratchpads across agents.\n\
                 Each connection is scoped to one project by the ?ws=<project path> \
                 query parameter on the server URL.\n\
                 - Registry: call identify (name, optional status) on startup so others \
                 can see you; whoami returns your own entry; agent_list returns every \
                 agent on the project. Connecting already registers you.\n\
                 - Locks: lock_acquire (name, optional note) takes a named advisory lock \
                 and is non-blocking; lock_release frees it; lock_status lists every held \
                 lock. Locks are advisory - agents cooperate, the daemon does not enforce.\n\
                 - Todos: todo_create (returns an id), todo_list (summaries), \
                 todo_get (one todo in full), todo_update (edit any field), \
                 todo_complete, todo_delete, todo_add_blocker / todo_remove_blocker, \
                 and todo_comment_add. Each todo also projects to .panopt/todos/<id>.md.\n\
                 - Scratchpads: scratchpad_create (returns an id), scratchpad_list, \
                 scratchpad_append, scratchpad_read. Reference scratchpads by numeric id.\n\
                 State is persisted, shared live across every agent on the same project, \
                 and mirrored into .panopt/*.md under the project root."
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::query_param;

    #[test]
    fn query_param_finds_each_key() {
        let q = Some("ws=/a/b&agent=alpha");
        assert_eq!(query_param(q, "ws").as_deref(), Some("/a/b"));
        assert_eq!(query_param(q, "agent").as_deref(), Some("alpha"));
    }

    #[test]
    fn query_param_percent_decodes() {
        assert_eq!(query_param(Some("ws=/a%20b"), "ws").as_deref(), Some("/a b"));
    }

    #[test]
    fn query_param_missing_or_no_query_is_none() {
        assert_eq!(query_param(Some("ws=/a"), "agent"), None);
        assert_eq!(query_param(None, "ws"), None);
    }

    #[test]
    fn query_param_requires_a_full_key_match() {
        // A key of "ws" must not be satisfied by a "wsx=" parameter.
        assert_eq!(query_param(Some("wsx=/a"), "ws"), None);
    }
}
