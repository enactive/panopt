//! The MCP server handler: the coordination tools and the `ServerHandler`
//! impl that advertises them.
//!
//! Every tool resolves its project from the `ws` query parameter on the
//! request URL (DESIGN.md Section 5.3) and registers the calling agent by its
//! agent key - the `agent` URL parameter when set, the MCP session id
//! otherwise - so one daemon serves every project at once with no per-session
//! state welded into the handler.
//!
//! The tool surface (names, descriptions, input schemas) is published by
//! [`panopt_tool_surface::TOOL_SURFACE`]. [`build_router`] iterates that
//! table at startup and registers each tool with the rmcp `ToolRouter` via
//! a closure that funnels every call through [`dispatch_local`]. The proxy
//! in `crates/panopt/src/mcp_proxy.rs` consumes the same table to answer
//! `tools/list` without ever talking to panoptd - that's the whole point of
//! the shared crate.

use std::sync::{Arc, Mutex};

use http::request::Parts;
use panopt_core::{
    Agent, AgentTool, AgentToolPatch, CoreError, KeySource, Lock, Priority, Process, ProcessKind,
    ProcessPatch, ProjectId, Scratchpad, ScratchpadPatch, Store, Todo, TodoPatch, TodoStatus,
};
use panopt_tool_surface::params::{
    AgentToolCreateArgs, AgentToolDeleteArgs, AgentToolGetArgs, AgentToolUpdateArgs, IdKindArgs,
    IdentifyArgs, LockAcquireArgs, LockReleaseArgs, ProcessCreateArgs, ProcessDeleteArgs,
    ProcessGetArgs, ProcessUpdateArgs, ScratchpadAppendArgs, ScratchpadCreateArgs,
    ScratchpadDeleteArgs, ScratchpadGetArgs, ScratchpadReadArgs, ScratchpadSearchArgs,
    ScratchpadUpdateArgs, TodoBlockerArgs, TodoCommentAddArgs, TodoCommentDeleteArgs,
    TodoCommentUpdateArgs, TodoCompleteArgs, TodoCreateArgs, TodoDeleteArgs, TodoGetArgs,
    TodoLockArgs, TodoSearchArgs, TodoSetBlockersArgs, TodoStartArgs, TodoUnlockArgs,
    TodoUpdateArgs,
};
use panopt_tool_surface::TOOL_SURFACE;
use rmcp::{
    handler::server::{
        router::tool::{ToolRoute, ToolRouter},
        tool::{parse_json_object, ToolCallContext},
    },
    model::*,
    ErrorData as McpError, ServerHandler,
};
use serde::Serialize;

/// Per-session MCP handler.
///
/// rmcp builds one of these per MCP session via the factory in `main`. Each
/// holds a *clone of the shared `Arc`*, so every session - and therefore every
/// connected agent - mutates and reads the one `Mutex<Store>`. The session
/// carries no project: each tool call derives it from the request URL.
#[derive(Clone)]
pub struct Handler {
    state: Arc<Mutex<Store>>,
    /// Built once in `new()` from [`build_router`] and consulted by this
    /// type's manual `ServerHandler` impl (see `call_tool`, `list_tools`).
    tool_router: ToolRouter<Self>,
}

/// Wire shape for a scratchpad in `scratchpad_list` output.
#[derive(Serialize)]
struct ScratchpadDto {
    id: u64,
    title: String,
}

/// Wire shape for `scratchpad_get`: title, body, tags, and timestamps.
#[derive(Serialize)]
struct ScratchpadDetailDto {
    id: u64,
    title: String,
    body: String,
    tags: Vec<String>,
    created_at: String,
    updated_at: String,
}

impl ScratchpadDetailDto {
    fn from_scratchpad(pad: &Scratchpad) -> Self {
        ScratchpadDetailDto {
            id: pad.id,
            title: pad.title.clone(),
            body: pad.body.clone(),
            tags: pad.tags.clone(),
            created_at: pad.created_at.clone(),
            updated_at: pad.updated_at.clone(),
        }
    }
}

/// Wire shape for `id_kind`: the resource kind and a short human label
/// (title for todos/scratchpads, `display_name` falling back to `name` for
/// agent tools and processes - the same fallback the cockpit's projection
/// uses).
#[derive(Serialize)]
struct IdKindDto {
    kind: &'static str,
    label: String,
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
    created_at: String,
    updated_at: String,
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
            created_at: todo.created_at.clone(),
            updated_at: todo.updated_at.clone(),
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
    /// Display name of the agent currently holding `todo:<id>` as an advisory
    /// lock; `None` when no agent holds it. Locks are ephemeral and not part of
    /// the SQLite-stored todo, so the daemon resolves this at read time.
    #[serde(skip_serializing_if = "Option::is_none")]
    locked_by: Option<String>,
}

impl TodoDetailDto {
    fn from_todo(todo: Todo, locked_by: Option<String>) -> Self {
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
            locked_by,
        }
    }
}

/// Wire shape for an agent tool (config layer) in `agent_tool_list` and
/// `agent_tool_get` output.
#[derive(Serialize)]
struct AgentToolDto {
    id: u64,
    name: String,
    display_name: String,
    command: String,
    cwd: String,
    tool_type: String,
    enabled: bool,
    position: i64,
    created_at: String,
}

impl AgentToolDto {
    fn from_entry(t: AgentTool) -> Self {
        AgentToolDto {
            id: t.id,
            name: t.name,
            display_name: t.display_name,
            command: t.command,
            cwd: t.cwd,
            tool_type: t.tool_type,
            enabled: t.enabled,
            position: t.position,
            created_at: t.created_at,
        }
    }
}

/// Wire shape for a process (instance layer) in `process_list` and
/// `process_get` output.
#[derive(Serialize)]
struct ProcessDto {
    id: u64,
    kind: &'static str,
    name: String,
    display_name: String,
    command: String,
    cwd: String,
    position: i64,
    agent_tool_id: Option<u64>,
    pid: Option<i64>,
    status: Option<String>,
    agent_state: Option<String>,
    last_seen: Option<String>,
    created_at: String,
}

impl ProcessDto {
    fn from_entry(p: Process) -> Self {
        ProcessDto {
            id: p.id,
            kind: p.kind.as_str(),
            name: p.name,
            display_name: p.display_name,
            command: p.command,
            cwd: p.cwd,
            position: p.position,
            agent_tool_id: p.agent_tool_id,
            pid: p.pid,
            status: p.status,
            agent_state: p.agent_state,
            last_seen: p.last_seen,
            created_at: p.created_at,
        }
    }
}

/// The display name of whoever holds `todo:<id>` in `project`, if anyone does.
/// Resolves through [`Store::lock_list`] so the holder's registered name lands
/// instead of the raw session key.
fn todo_lock_holder(store: &Store, project: ProjectId, todo_id: u64) -> Option<String> {
    let name = format!("todo:{todo_id}");
    store
        .lock_list(project)
        .into_iter()
        .find(|l| l.name == name)
        .map(|l| l.holder_name)
}

/// Wire shape for an agent in `agent_list` and `whoami` output.
#[derive(Serialize)]
struct AgentDto {
    name: String,
    status: String,
    /// Seconds since this agent's last tool call.
    idle_seconds: u64,
    /// `"declared"` for stable `?agent=<id>` identities, `"session"` for the
    /// rotating MCP session id. Lets a peer tell whether an idle entry will
    /// auto-prune (session) or persist until explicit leave (declared).
    key_source: &'static str,
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
            key_source: agent.key_source.as_str(),
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

/// Probe the four resource tables in turn for `id`, returning the first hit
/// as an [`IdKindDto`].
///
/// Per-table `NotFound` is treated as "miss, try next"; any other `CoreError`
/// (DB, projection, project-not-found) short-circuits as a real failure. If
/// no table matches, emits `invalid_params` with a generic "id N not found"
/// (the caller doesn't have a kind to attribute the miss to). Soft-deleted
/// rows are already invisible to the underlying `*_get` helpers (V7), so
/// they look the same as ids that were never allocated.
fn resolve_id_kind(store: &Store, project: ProjectId, id: u64) -> Result<IdKindDto, McpError> {
    match store.todo_get(project, id) {
        Ok(t) => {
            return Ok(IdKindDto {
                kind: "todo",
                label: t.title,
            })
        }
        Err(CoreError::TodoNotFound(_)) => {}
        Err(e) => return Err(map_core_err(e)),
    }
    match store.scratchpad_get(project, id) {
        Ok(s) => {
            return Ok(IdKindDto {
                kind: "scratchpad",
                label: s.title,
            })
        }
        Err(CoreError::ScratchpadNotFound(_)) => {}
        Err(e) => return Err(map_core_err(e)),
    }
    match store.agent_tool_get(project, id) {
        Ok(at) => {
            let label = if at.display_name.is_empty() {
                at.name
            } else {
                at.display_name
            };
            return Ok(IdKindDto {
                kind: "agent-tool",
                label,
            });
        }
        Err(CoreError::AgentToolNotFound(_)) => {}
        Err(e) => return Err(map_core_err(e)),
    }
    match store.process_get(project, id) {
        Ok(p) => {
            let label = if p.display_name.is_empty() {
                p.name
            } else {
                p.display_name
            };
            return Ok(IdKindDto {
                kind: "process",
                label,
            });
        }
        Err(CoreError::ProcessNotFound(_)) => {}
        Err(e) => return Err(map_core_err(e)),
    }
    Err(McpError::invalid_params(format!("id {id} not found"), None))
}

/// Map a core error onto an MCP error result at the protocol boundary.
fn map_core_err(e: CoreError) -> McpError {
    match e {
        // Caller-fixable: a bad id, a rejected argument, or a workspace path
        // the daemon cannot reach.
        CoreError::ScratchpadNotFound(_)
        | CoreError::TodoNotFound(_)
        | CoreError::TodoCommentNotFound { .. }
        | CoreError::AgentToolNotFound(_)
        | CoreError::ProcessNotFound(_)
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

/// The calling agent's stable key and the source it came from.
///
/// Prefers the `agent` query parameter on the MCP URL - a stable per-agent id
/// the launcher injects, which survives MCP session churn. Falls back to the
/// `mcp-session-id` header when no `agent` is set; that header rotates whenever
/// the client's connection drops and re-initializes, so without an `agent` id
/// one agent can briefly appear under several keys (see DESIGN.md Section 9).
/// `None` only for a request that carries neither.
///
/// The returned [`KeySource`] tells the registry how to age the entry:
/// declared identities never idle-prune, session ids do.
fn agent_key(parts: &Parts) -> Option<(String, KeySource)> {
    if let Some(id) = query_param(parts.uri.query(), "agent").filter(|id| !id.is_empty()) {
        return Some((id, KeySource::Declared));
    }
    parts
        .headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| (s.to_string(), KeySource::Session))
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
///
/// On first-sight of a stable `?agent=` key, a `?name=<friendly>` parameter
/// on the URL is applied as an implicit `agent_identify`. This lets a
/// hand-launched agent declare its display name once in the MCP URL instead
/// of needing to call `identify` on every reconnect.
fn enter(store: &mut Store, parts: &Parts) -> Result<(ProjectId, Option<String>), McpError> {
    let project = resolve_project(store, parts)?;
    if query_param(parts.uri.query(), "observer").as_deref() == Some("1") {
        return Ok((project, None));
    }
    let Some((key, source)) = agent_key(parts) else {
        return Ok((project, None));
    };
    let first_seen = store.agent_whoami(project, &key).is_none();
    store
        .agent_touch(project, &key, source)
        .map_err(map_core_err)?;
    if first_seen {
        tracing::info!(
            agent = %key,
            key_source = source.as_str(),
            "agent connected"
        );
        if let Some(name) = query_param(parts.uri.query(), "name").filter(|n| !n.is_empty()) {
            store
                .agent_identify(project, &key, name.clone(), None)
                .map_err(map_core_err)?;
            tracing::info!(agent = %key, %name, "agent identified from URL");
        }
    }
    Ok((project, Some(key)))
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
        return Err(McpError::invalid_params(
            "lock name must not be empty",
            None,
        ));
    }
    Ok(name)
}

/// Parse a status token from a tool argument, with a caller-facing error.
fn parse_status(s: &str) -> Result<TodoStatus, McpError> {
    TodoStatus::parse(s).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "invalid status '{s}': expected open, in_progress, backlog, draft, completed, or not_done"
            ),
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

/// Parse a process-kind token from a tool argument, with a caller-facing error.
fn parse_process_kind(s: &str) -> Result<ProcessKind, McpError> {
    ProcessKind::parse(s).ok_or_else(|| {
        McpError::invalid_params(
            format!("invalid kind '{s}': expected agent, command, or terminal"),
            None,
        )
    })
}

fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json =
        serde_json::to_string(value).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

impl Handler {
    pub fn new(state: Arc<Mutex<Store>>) -> Self {
        Self {
            state,
            tool_router: build_router(),
        }
    }

    async fn identify(&self, parts: Parts, args: IdentifyArgs) -> Result<CallToolResult, McpError> {
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

    async fn agent_leave(&self, parts: Parts) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            let removed = st.agent_leave(project, &key).map_err(map_core_err)?;
            tracing::info!(agent = %key, removed, "agent left");
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn whoami(&self, parts: Parts) -> Result<CallToolResult, McpError> {
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

    async fn agent_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
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

    async fn lock_acquire(
        &self,
        parts: Parts,
        args: LockAcquireArgs,
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
            Some(holder) => {
                json_result(&serde_json::json!({ "acquired": false, "held_by": holder }))
            }
        }
    }

    async fn lock_release(
        &self,
        parts: Parts,
        args: LockReleaseArgs,
    ) -> Result<CallToolResult, McpError> {
        let name = require_lock_name(args.name)?;
        let outcome = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            st.lock_release(project, &key, &name)
                .map_err(map_core_err)?
        };
        match outcome {
            None => json_result(&serde_json::json!({ "released": true })),
            Some(holder) => {
                json_result(&serde_json::json!({ "released": false, "held_by": holder }))
            }
        }
    }

    async fn lock_status(&self, parts: Parts) -> Result<CallToolResult, McpError> {
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

    async fn scratchpad_create(
        &self,
        parts: Parts,
        args: ScratchpadCreateArgs,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_create(project, args.title)
                .map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }

    async fn scratchpad_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
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

    async fn scratchpad_search(
        &self,
        parts: Parts,
        args: ScratchpadSearchArgs,
    ) -> Result<CallToolResult, McpError> {
        let dtos: Vec<ScratchpadDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            let tags = args.tags.unwrap_or_default();
            st.scratchpad_search(project, args.query.as_deref(), &tags)
                .map_err(map_core_err)?
                .into_iter()
                .map(|(id, title)| ScratchpadDto { id, title })
                .collect()
        };
        json_result(&dtos)
    }

    async fn scratchpad_append(
        &self,
        parts: Parts,
        args: ScratchpadAppendArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_append(project, args.scratchpad_id, &args.content)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn scratchpad_read(
        &self,
        parts: Parts,
        args: ScratchpadReadArgs,
    ) -> Result<CallToolResult, McpError> {
        let body = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_read(project, args.scratchpad_id)
                .map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    async fn scratchpad_get(
        &self,
        parts: Parts,
        args: ScratchpadGetArgs,
    ) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            let pad = st
                .scratchpad_get(project, args.scratchpad_id)
                .map_err(map_core_err)?;
            ScratchpadDetailDto::from_scratchpad(&pad)
        };
        json_result(&dto)
    }

    async fn scratchpad_update(
        &self,
        parts: Parts,
        args: ScratchpadUpdateArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            let patch = ScratchpadPatch {
                title: args.title,
                body: args.body,
                tags: args.tags,
            };
            st.scratchpad_update(project, args.scratchpad_id, patch)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn scratchpad_delete(
        &self,
        parts: Parts,
        args: ScratchpadDeleteArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.scratchpad_delete(project, args.scratchpad_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn scratchpad_tags_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
        let tags = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.tags_list(project).map_err(map_core_err)?
        };
        json_result(&tags)
    }

    async fn todo_create(
        &self,
        parts: Parts,
        args: TodoCreateArgs,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_create(project, args.title).map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }

    async fn todo_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
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

    async fn todo_search(
        &self,
        parts: Parts,
        args: TodoSearchArgs,
    ) -> Result<CallToolResult, McpError> {
        let status = match &args.status {
            Some(s) => Some(parse_status(s)?),
            None => None,
        };
        let priority = match &args.priority {
            Some(p) => Some(parse_priority(p)?),
            None => None,
        };
        let tags = args.tags.unwrap_or_default();
        let dtos: Vec<TodoSummaryDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_search(
                project,
                args.query.as_deref(),
                status,
                priority,
                args.assignee.as_deref(),
                &tags,
            )
            .map_err(map_core_err)?
            .iter()
            .map(TodoSummaryDto::from_todo)
            .collect()
        };
        json_result(&dtos)
    }

    async fn todo_get(&self, parts: Parts, args: TodoGetArgs) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            let todo = st.todo_get(project, args.todo_id).map_err(map_core_err)?;
            let locked_by = todo_lock_holder(&st, project, args.todo_id);
            TodoDetailDto::from_todo(todo, locked_by)
        };
        json_result(&dto)
    }

    async fn todo_update(
        &self,
        parts: Parts,
        args: TodoUpdateArgs,
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
            st.todo_update(project, args.todo_id, patch)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_complete(
        &self,
        parts: Parts,
        args: TodoCompleteArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_complete(project, args.todo_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_start(
        &self,
        parts: Parts,
        args: TodoStartArgs,
    ) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().expect("state mutex poisoned");
        let (project, key) = enter(&mut st, &parts)?;
        let key = require_key(key)?;
        // Make sure the todo exists before we touch the lock table - otherwise
        // an agent could squat `todo:999` forever (same guard `todo_lock` has).
        st.todo_get(project, args.todo_id).map_err(map_core_err)?;
        let name = format!("todo:{}", args.todo_id);
        match st
            .lock_acquire(project, &key, name, args.note)
            .map_err(map_core_err)?
        {
            Some(holder) => {
                json_result(&serde_json::json!({ "started": false, "held_by": holder }))
            }
            None => {
                st.todo_start(project, args.todo_id).map_err(map_core_err)?;
                let todo = st.todo_get(project, args.todo_id).map_err(map_core_err)?;
                let locked_by = todo_lock_holder(&st, project, args.todo_id);
                let dto = TodoDetailDto::from_todo(todo, locked_by);
                json_result(&serde_json::json!({ "started": true, "todo": dto }))
            }
        }
    }

    async fn todo_delete(
        &self,
        parts: Parts,
        args: TodoDeleteArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_delete(project, args.todo_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_add_blocker(
        &self,
        parts: Parts,
        args: TodoBlockerArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_add_blocker(project, args.todo_id, args.blocker_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_remove_blocker(
        &self,
        parts: Parts,
        args: TodoBlockerArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_remove_blocker(project, args.todo_id, args.blocker_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_comment_add(
        &self,
        parts: Parts,
        args: TodoCommentAddArgs,
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

    async fn todo_comment_update(
        &self,
        parts: Parts,
        args: TodoCommentUpdateArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_comment_update(project, args.todo_id, args.comment_id, args.body)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_comment_delete(
        &self,
        parts: Parts,
        args: TodoCommentDeleteArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_comment_delete(project, args.todo_id, args.comment_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_set_blockers(
        &self,
        parts: Parts,
        args: TodoSetBlockersArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.todo_set_blockers(project, args.todo_id, args.blocker_ids)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn todo_tags_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
        let tags = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.tags_list(project).map_err(map_core_err)?
        };
        json_result(&tags)
    }

    async fn todo_lock(
        &self,
        parts: Parts,
        args: TodoLockArgs,
    ) -> Result<CallToolResult, McpError> {
        let outcome = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            // Make sure the todo exists before we register a lock against it -
            // otherwise an agent could squat `todo:999` forever.
            st.todo_get(project, args.todo_id).map_err(map_core_err)?;
            let name = format!("todo:{}", args.todo_id);
            st.lock_acquire(project, &key, name, args.note)
                .map_err(map_core_err)?
        };
        match outcome {
            None => json_result(&serde_json::json!({ "acquired": true })),
            Some(holder) => {
                json_result(&serde_json::json!({ "acquired": false, "held_by": holder }))
            }
        }
    }

    async fn todo_unlock(
        &self,
        parts: Parts,
        args: TodoUnlockArgs,
    ) -> Result<CallToolResult, McpError> {
        let outcome = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, key) = enter(&mut st, &parts)?;
            let key = require_key(key)?;
            let name = format!("todo:{}", args.todo_id);
            st.lock_release(project, &key, &name)
                .map_err(map_core_err)?
        };
        match outcome {
            None => json_result(&serde_json::json!({ "released": true })),
            Some(holder) => {
                json_result(&serde_json::json!({ "released": false, "held_by": holder }))
            }
        }
    }

    async fn agent_tool_create(
        &self,
        parts: Parts,
        args: AgentToolCreateArgs,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.agent_tool_create(
                project,
                args.name,
                args.display_name.unwrap_or_default(),
                args.command.unwrap_or_default(),
                args.cwd.unwrap_or_default(),
                args.tool_type.unwrap_or_else(|| "agent".to_string()),
                args.enabled.unwrap_or(true),
            )
            .map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }

    async fn agent_tool_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
        let dtos: Vec<AgentToolDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.agent_tool_list(project)
                .map_err(map_core_err)?
                .into_iter()
                .map(AgentToolDto::from_entry)
                .collect()
        };
        json_result(&dtos)
    }

    async fn agent_tool_get(
        &self,
        parts: Parts,
        args: AgentToolGetArgs,
    ) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            AgentToolDto::from_entry(
                st.agent_tool_get(project, args.agent_tool_id)
                    .map_err(map_core_err)?,
            )
        };
        json_result(&dto)
    }

    async fn agent_tool_update(
        &self,
        parts: Parts,
        args: AgentToolUpdateArgs,
    ) -> Result<CallToolResult, McpError> {
        let patch = AgentToolPatch {
            name: args.name,
            display_name: args.display_name,
            command: args.command,
            cwd: args.cwd,
            tool_type: args.tool_type,
            enabled: args.enabled,
            position: args.position,
        };
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.agent_tool_update(project, args.agent_tool_id, patch)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn agent_tool_delete(
        &self,
        parts: Parts,
        args: AgentToolDeleteArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.agent_tool_delete(project, args.agent_tool_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn process_create(
        &self,
        parts: Parts,
        args: ProcessCreateArgs,
    ) -> Result<CallToolResult, McpError> {
        let kind = parse_process_kind(&args.kind)?;
        let id = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.process_create(
                project,
                kind,
                args.name,
                args.display_name.unwrap_or_default(),
                args.command.unwrap_or_default(),
                args.cwd.unwrap_or_default(),
                args.agent_tool_id,
            )
            .map_err(map_core_err)?
        };
        Ok(CallToolResult::success(vec![Content::text(id.to_string())]))
    }

    async fn process_list(&self, parts: Parts) -> Result<CallToolResult, McpError> {
        let dtos: Vec<ProcessDto> = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.process_list(project)
                .map_err(map_core_err)?
                .into_iter()
                .map(ProcessDto::from_entry)
                .collect()
        };
        json_result(&dtos)
    }

    async fn process_get(
        &self,
        parts: Parts,
        args: ProcessGetArgs,
    ) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            ProcessDto::from_entry(
                st.process_get(project, args.process_id)
                    .map_err(map_core_err)?,
            )
        };
        json_result(&dto)
    }

    async fn process_update(
        &self,
        parts: Parts,
        args: ProcessUpdateArgs,
    ) -> Result<CallToolResult, McpError> {
        let patch = ProcessPatch {
            name: args.name,
            display_name: args.display_name,
            command: args.command,
            cwd: args.cwd,
            position: args.position,
            ..Default::default()
        };
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.process_update(project, args.process_id, patch)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn process_delete(
        &self,
        parts: Parts,
        args: ProcessDeleteArgs,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            st.process_delete(project, args.process_id)
                .map_err(map_core_err)?;
        }
        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    async fn id_kind(&self, parts: Parts, args: IdKindArgs) -> Result<CallToolResult, McpError> {
        let dto = {
            let mut st = self.state.lock().expect("state mutex poisoned");
            let (project, _) = enter(&mut st, &parts)?;
            resolve_id_kind(&st, project, args.id)?
        };
        json_result(&dto)
    }
}

/// Every tool name in [`TOOL_SURFACE`], promoted to an enum so [`dispatch_local`]'s
/// `match` is exhaustive: adding a new tool here forces a corresponding
/// dispatch arm at compile time. The unit test `tool_enum_covers_tool_surface`
/// asserts the inverse - that every surface entry resolves through [`Tool::from_name`].
#[derive(Clone, Copy)]
enum Tool {
    Identify,
    AgentLeave,
    Whoami,
    AgentList,
    LockAcquire,
    LockRelease,
    LockStatus,
    ScratchpadCreate,
    ScratchpadList,
    ScratchpadSearch,
    ScratchpadAppend,
    ScratchpadRead,
    ScratchpadGet,
    ScratchpadUpdate,
    ScratchpadDelete,
    ScratchpadTagsList,
    TodoCreate,
    TodoList,
    TodoSearch,
    TodoGet,
    TodoUpdate,
    TodoComplete,
    TodoStart,
    TodoDelete,
    TodoAddBlocker,
    TodoRemoveBlocker,
    TodoCommentAdd,
    TodoCommentUpdate,
    TodoCommentDelete,
    TodoSetBlockers,
    TodoTagsList,
    TodoLock,
    TodoUnlock,
    AgentToolCreate,
    AgentToolList,
    AgentToolGet,
    AgentToolUpdate,
    AgentToolDelete,
    ProcessCreate,
    ProcessList,
    ProcessGet,
    ProcessUpdate,
    ProcessDelete,
    IdKind,
}

impl Tool {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "identify" => Tool::Identify,
            "agent_leave" => Tool::AgentLeave,
            "whoami" => Tool::Whoami,
            "agent_list" => Tool::AgentList,
            "lock_acquire" => Tool::LockAcquire,
            "lock_release" => Tool::LockRelease,
            "lock_status" => Tool::LockStatus,
            "scratchpad_create" => Tool::ScratchpadCreate,
            "scratchpad_list" => Tool::ScratchpadList,
            "scratchpad_search" => Tool::ScratchpadSearch,
            "scratchpad_append" => Tool::ScratchpadAppend,
            "scratchpad_read" => Tool::ScratchpadRead,
            "scratchpad_get" => Tool::ScratchpadGet,
            "scratchpad_update" => Tool::ScratchpadUpdate,
            "scratchpad_delete" => Tool::ScratchpadDelete,
            "scratchpad_tags_list" => Tool::ScratchpadTagsList,
            "todo_create" => Tool::TodoCreate,
            "todo_list" => Tool::TodoList,
            "todo_search" => Tool::TodoSearch,
            "todo_get" => Tool::TodoGet,
            "todo_update" => Tool::TodoUpdate,
            "todo_complete" => Tool::TodoComplete,
            "todo_start" => Tool::TodoStart,
            "todo_delete" => Tool::TodoDelete,
            "todo_add_blocker" => Tool::TodoAddBlocker,
            "todo_remove_blocker" => Tool::TodoRemoveBlocker,
            "todo_comment_add" => Tool::TodoCommentAdd,
            "todo_comment_update" => Tool::TodoCommentUpdate,
            "todo_comment_delete" => Tool::TodoCommentDelete,
            "todo_set_blockers" => Tool::TodoSetBlockers,
            "todo_tags_list" => Tool::TodoTagsList,
            "todo_lock" => Tool::TodoLock,
            "todo_unlock" => Tool::TodoUnlock,
            "agent_tool_create" => Tool::AgentToolCreate,
            "agent_tool_list" => Tool::AgentToolList,
            "agent_tool_get" => Tool::AgentToolGet,
            "agent_tool_update" => Tool::AgentToolUpdate,
            "agent_tool_delete" => Tool::AgentToolDelete,
            "process_create" => Tool::ProcessCreate,
            "process_list" => Tool::ProcessList,
            "process_get" => Tool::ProcessGet,
            "process_update" => Tool::ProcessUpdate,
            "process_delete" => Tool::ProcessDelete,
            "id_kind" => Tool::IdKind,
            _ => return None,
        })
    }
}

/// Build the rmcp `ToolRouter` from [`TOOL_SURFACE`].
///
/// One `add_route` per entry. The handler closure is uniform: every tool
/// funnels through [`dispatch_local`], which keys off the tool name. This is
/// the only place rmcp's `ToolRoute`/`Tool` types are constructed, so any
/// schema-shape surprise from the shared crate surfaces here at registration
/// rather than at first-call.
fn build_router() -> ToolRouter<Handler> {
    let mut router = ToolRouter::<Handler>::new();
    for def in TOOL_SURFACE {
        let name: &'static str = def.name;
        let schema_obj = match (def.schema_fn)() {
            serde_json::Value::Object(m) => m,
            other => panic!("TOOL_SURFACE entry `{name}` produced a non-object schema: {other:?}"),
        };
        let attr = rmcp::model::Tool::new(name, def.description, Arc::new(schema_obj));
        router.add_route(ToolRoute::new_dyn(attr, move |ctx| {
            Box::pin(dispatch_local(name, ctx))
        }));
    }
    router
}

/// Route an incoming tool call to its impl on [`Handler`].
///
/// Pulls the `http::request::Parts` extension that `main.rs` injects per
/// request, deserializes the JSON arguments into the tool's `Parameters<T>`
/// type from `panopt_tool_surface::params`, then calls the corresponding
/// async method on `Handler`. The `match` over [`Tool`] is exhaustive, so the
/// compiler rejects a new tool added without a dispatch arm.
async fn dispatch_local<'a>(
    name: &'static str,
    ctx: ToolCallContext<'a, Handler>,
) -> Result<CallToolResult, McpError> {
    let tool = Tool::from_name(name)
        .ok_or_else(|| McpError::invalid_params(format!("unknown tool `{name}`"), None))?;
    let parts = ctx
        .request_context
        .extensions
        .get::<Parts>()
        .cloned()
        .ok_or_else(|| {
            McpError::internal_error(
                "the request is missing its http::request::Parts extension",
                None,
            )
        })?;
    let handler = ctx.service;
    let raw_args = ctx.arguments.unwrap_or_default();
    match tool {
        Tool::Identify => {
            let args: IdentifyArgs = parse_json_object(raw_args)?;
            handler.identify(parts, args).await
        }
        Tool::AgentLeave => handler.agent_leave(parts).await,
        Tool::Whoami => handler.whoami(parts).await,
        Tool::AgentList => handler.agent_list(parts).await,
        Tool::LockAcquire => {
            let args: LockAcquireArgs = parse_json_object(raw_args)?;
            handler.lock_acquire(parts, args).await
        }
        Tool::LockRelease => {
            let args: LockReleaseArgs = parse_json_object(raw_args)?;
            handler.lock_release(parts, args).await
        }
        Tool::LockStatus => handler.lock_status(parts).await,
        Tool::ScratchpadCreate => {
            let args: ScratchpadCreateArgs = parse_json_object(raw_args)?;
            handler.scratchpad_create(parts, args).await
        }
        Tool::ScratchpadList => handler.scratchpad_list(parts).await,
        Tool::ScratchpadSearch => {
            let args: ScratchpadSearchArgs = parse_json_object(raw_args)?;
            handler.scratchpad_search(parts, args).await
        }
        Tool::ScratchpadAppend => {
            let args: ScratchpadAppendArgs = parse_json_object(raw_args)?;
            handler.scratchpad_append(parts, args).await
        }
        Tool::ScratchpadRead => {
            let args: ScratchpadReadArgs = parse_json_object(raw_args)?;
            handler.scratchpad_read(parts, args).await
        }
        Tool::ScratchpadGet => {
            let args: ScratchpadGetArgs = parse_json_object(raw_args)?;
            handler.scratchpad_get(parts, args).await
        }
        Tool::ScratchpadUpdate => {
            let args: ScratchpadUpdateArgs = parse_json_object(raw_args)?;
            handler.scratchpad_update(parts, args).await
        }
        Tool::ScratchpadDelete => {
            let args: ScratchpadDeleteArgs = parse_json_object(raw_args)?;
            handler.scratchpad_delete(parts, args).await
        }
        Tool::ScratchpadTagsList => handler.scratchpad_tags_list(parts).await,
        Tool::TodoCreate => {
            let args: TodoCreateArgs = parse_json_object(raw_args)?;
            handler.todo_create(parts, args).await
        }
        Tool::TodoList => handler.todo_list(parts).await,
        Tool::TodoSearch => {
            let args: TodoSearchArgs = parse_json_object(raw_args)?;
            handler.todo_search(parts, args).await
        }
        Tool::TodoGet => {
            let args: TodoGetArgs = parse_json_object(raw_args)?;
            handler.todo_get(parts, args).await
        }
        Tool::TodoUpdate => {
            let args: TodoUpdateArgs = parse_json_object(raw_args)?;
            handler.todo_update(parts, args).await
        }
        Tool::TodoComplete => {
            let args: TodoCompleteArgs = parse_json_object(raw_args)?;
            handler.todo_complete(parts, args).await
        }
        Tool::TodoStart => {
            let args: TodoStartArgs = parse_json_object(raw_args)?;
            handler.todo_start(parts, args).await
        }
        Tool::TodoDelete => {
            let args: TodoDeleteArgs = parse_json_object(raw_args)?;
            handler.todo_delete(parts, args).await
        }
        Tool::TodoAddBlocker => {
            let args: TodoBlockerArgs = parse_json_object(raw_args)?;
            handler.todo_add_blocker(parts, args).await
        }
        Tool::TodoRemoveBlocker => {
            let args: TodoBlockerArgs = parse_json_object(raw_args)?;
            handler.todo_remove_blocker(parts, args).await
        }
        Tool::TodoCommentAdd => {
            let args: TodoCommentAddArgs = parse_json_object(raw_args)?;
            handler.todo_comment_add(parts, args).await
        }
        Tool::TodoCommentUpdate => {
            let args: TodoCommentUpdateArgs = parse_json_object(raw_args)?;
            handler.todo_comment_update(parts, args).await
        }
        Tool::TodoCommentDelete => {
            let args: TodoCommentDeleteArgs = parse_json_object(raw_args)?;
            handler.todo_comment_delete(parts, args).await
        }
        Tool::TodoSetBlockers => {
            let args: TodoSetBlockersArgs = parse_json_object(raw_args)?;
            handler.todo_set_blockers(parts, args).await
        }
        Tool::TodoTagsList => handler.todo_tags_list(parts).await,
        Tool::TodoLock => {
            let args: TodoLockArgs = parse_json_object(raw_args)?;
            handler.todo_lock(parts, args).await
        }
        Tool::TodoUnlock => {
            let args: TodoUnlockArgs = parse_json_object(raw_args)?;
            handler.todo_unlock(parts, args).await
        }
        Tool::AgentToolCreate => {
            let args: AgentToolCreateArgs = parse_json_object(raw_args)?;
            handler.agent_tool_create(parts, args).await
        }
        Tool::AgentToolList => handler.agent_tool_list(parts).await,
        Tool::AgentToolGet => {
            let args: AgentToolGetArgs = parse_json_object(raw_args)?;
            handler.agent_tool_get(parts, args).await
        }
        Tool::AgentToolUpdate => {
            let args: AgentToolUpdateArgs = parse_json_object(raw_args)?;
            handler.agent_tool_update(parts, args).await
        }
        Tool::AgentToolDelete => {
            let args: AgentToolDeleteArgs = parse_json_object(raw_args)?;
            handler.agent_tool_delete(parts, args).await
        }
        Tool::ProcessCreate => {
            let args: ProcessCreateArgs = parse_json_object(raw_args)?;
            handler.process_create(parts, args).await
        }
        Tool::ProcessList => handler.process_list(parts).await,
        Tool::ProcessGet => {
            let args: ProcessGetArgs = parse_json_object(raw_args)?;
            handler.process_get(parts, args).await
        }
        Tool::ProcessUpdate => {
            let args: ProcessUpdateArgs = parse_json_object(raw_args)?;
            handler.process_update(parts, args).await
        }
        Tool::ProcessDelete => {
            let args: ProcessDeleteArgs = parse_json_object(raw_args)?;
            handler.process_delete(parts, args).await
        }
        Tool::IdKind => {
            let args: IdKindArgs = parse_json_object(raw_args)?;
            handler.id_kind(parts, args).await
        }
    }
}

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
                 agent on the project (each entry's key_source is `declared` for stable \
                 `?agent=<id>` ids or `session` for the rotating MCP session id - only \
                 session entries idle-prune). Connecting already registers you. \
                 agent_leave removes your registry entry and releases your locks; the \
                 idle sweep handles agents that just disappear.\n\
                 - Locks: lock_acquire (name, optional note) takes a named advisory lock \
                 and is non-blocking; lock_release frees it; lock_status lists every held \
                 lock. Locks are advisory - agents cooperate, the daemon does not enforce.\n\
                 - Todos: todo_create (returns an id), todo_list (summaries), \
                 todo_get (one todo in full), todo_update (edit any field), \
                 todo_complete, todo_delete, todo_add_blocker / todo_remove_blocker / \
                 todo_set_blockers, todo_comment_add / todo_comment_update / \
                 todo_comment_delete, todo_tags_list (project tag vocabulary, union with \
                 scratchpads), and todo_lock / todo_unlock (advisory `todo:<id>` lock, \
                 surfaced as locked_by on todo_get). Each todo also projects to \
                 .panopt/todos/<id>.md.\n\
                 - Scratchpads: scratchpad_create (returns an id), scratchpad_list, \
                 scratchpad_get (one scratchpad in full), scratchpad_append (add to body), \
                 scratchpad_read (body only), scratchpad_update (replace title, body, and/or \
                 tags), scratchpad_delete, scratchpad_tags_list (project tag vocabulary, \
                 same union as todo_tags_list). Each scratchpad also projects to \
                 .panopt/scratchpad/<id>.md.\n\
                 - Agent tools (config layer): agent_tool_create (returns an id), \
                 agent_tool_list, agent_tool_get, agent_tool_update, agent_tool_delete. \
                 Durable per-project agent configurations the cockpit can spawn from. \
                 Projected to .panopt/agent_tools.md.\n\
                 - Processes (instance layer): process_create (kind agent/command/terminal, \
                 optional agent_tool_id, returns an id), process_list, process_get, \
                 process_update, process_delete. Per-project process instances - the \
                 launchable agents, commands, and terminals the cockpit tracks. Deleting an \
                 agent tool nulls the agent_tool_id back-reference of any processes that \
                 referenced it. Projected to .panopt/processes.md.\n\
                 - Utilities: id_kind resolves a numeric id to its resource kind \
                 (todo / scratchpad / agent-tool / process) plus a short label. \
                 Useful since ids are unified per project and a `#N` reference \
                 points to exactly one row.\n\
                 State is persisted, shared live across every agent on the same project, \
                 and mirrored into .panopt/*.md under the project root."
                    .to_string(),
            )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tool_router.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::{query_param, Tool};
    use panopt_tool_surface::TOOL_SURFACE;

    /// Coverage check: every tool published in [`TOOL_SURFACE`] resolves
    /// through [`Tool::from_name`]. Combined with the exhaustive `match Tool`
    /// in `dispatch_local`, this guarantees every published tool has a
    /// dispatch arm - so adding a `TOOL_SURFACE` entry without extending
    /// `Tool` and `Tool::from_name` fails this test, and adding a `Tool`
    /// variant without a `dispatch_local` arm fails to compile.
    #[test]
    fn tool_enum_covers_tool_surface() {
        for def in TOOL_SURFACE {
            assert!(
                Tool::from_name(def.name).is_some(),
                "TOOL_SURFACE entry `{}` has no corresponding `Tool` enum variant",
                def.name
            );
        }
    }

    #[test]
    fn query_param_finds_each_key() {
        let q = Some("ws=/a/b&agent=alpha");
        assert_eq!(query_param(q, "ws").as_deref(), Some("/a/b"));
        assert_eq!(query_param(q, "agent").as_deref(), Some("alpha"));
    }

    #[test]
    fn query_param_percent_decodes() {
        assert_eq!(
            query_param(Some("ws=/a%20b"), "ws").as_deref(),
            Some("/a b")
        );
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
