# PANopt - Design Document

Status: Draft
Date: 2026-05-26
Location: `~/p/panopt`

## 1. Overview

PANopt is a personal, cross-platform tool that replicates the multi-agent
coordination workflow of Solo (soloterm) without being a terminal emulator or a
code editor itself.

In one line: **PANopt is a coordination daemon.** Agents run as terminal
sessions in Zellij; PANopt is the shared brain they talk to - a single MCP
server holding todos, notes, locks, and an agent registry, surfaced live
as a Zellij sidebar plugin and as ordinary projected files.

The name fits the design. A panopticon is a structure with a single vantage
point from which every cell is observed at once. PANopt's daemon is exactly
that vantage point: one process from which every agent, todo, and note is
observed and coordinated. The architecture is not incidental to the name - the
single central observer *is* the product.

## 2. Background and Motivation

Solo (soloterm) is a macOS-only application, built with Rust and Tauri (a Rust
core plus a web frontend in a system webview). It does three things:

1. Wraps agent CLIs in a GUI - launch and manage multiple terminal-based
   agents from one window.
2. Runs an MCP server for inter-agent coordination.
3. Provides shared todos and notes across those agents.

The user runs Linux and wants this workflow there. PANopt itself is not
platform-bound, though: it is portable Rust, verified end-to-end on macOS, with
Linux as its primary deployment target. PANopt is a personal tool: no licensing,
telemetry, hosted backend, auto-update, built-in chat, or pipelines. It does not 
need to be a polished, shippable product - it needs to deliver the workflow.

This project was originally conceived as "solow", a pinned fork of the Ghostty
terminal emulator in Zig. That approach has been superseded. Section 4 records
why in full, because the reasoning is the most important content in this
document.

## 3. Goals and Non-Goals

### Goals

- Inter-agent coordination: multiple agent CLIs sharing todos, notes,
  advisory locks, and a live agent registry.
- Todos and notes that feel first-class in the cockpit.
- Minimal maintenance burden. One maintainer, personal tool, indefinite
  lifespan.
- Work with the tools that already exist (Zellij, MCP-capable agent CLIs)
  instead of rebuilding them.
- Transport-agnostic coordination: the core must not be welded to any one host
  application.

### Non-Goals

- Not a terminal emulator.
- Not a code editor.
- No standalone GUI application and no single bespoke window.
- Not a pixel-faithful clone of Solo's UI.
- No licensing, telemetry, hosted backend, auto-update, built-in chat, or
  pipelines.

## 4. Architecture Decision: a daemon, not a fork

This section records every option considered and why it was accepted or
rejected. It is deliberately the longest section.

### 4.1 Understanding the original Solo

To replicate Solo's behavior, PANopt needs Solo's MCP surface, its on-disk
formats for todos and notes, and its process model.

The correct approach is dynamic, black-box observation, which is cheap because
Tauri apps expose almost everything:

- The frontend (HTML/CSS/JS) is bundled into the app and is readable as-is.
  Every `invoke()` call in that JS names a Rust IPC command, so the frontend
  enumerates the core API for free.
- `strings` on the Rust core is productive: panic messages embed source paths,
  `serde` keeps field names as literals, and `#[derive(Debug)]` leaves struct
  and field names in the binary.
- `fs_usage` and `lsof` while Solo runs reveal the on-disk formats directly.
- MCP is an open JSON-RPC protocol; pointing an MCP client at Solo's server
  exposes the coordination surface with no reverse engineering at all.

PANopt's specification is therefore derived from observing Solo, not from
decompiling it.

### 4.2 Option A - fork a terminal emulator (Ghostty)

The original plan: fork Ghostty and add Solo's functions on top.

Rejected. Forking a terminal emulator means owning a divergent copy of a large
codebase - font rendering, GPU, input handling, all of it commodity - and
rebasing it onto upstream forever. That is a heavy, permanent maintenance cost
for a personal tool whose top goal is minimal maintenance.

The multi-agent terminal UI the fork was meant to deliver is not worth forking
for: a finished terminal multiplexer already provides multi-agent pane and tab
management as an off-the-shelf, unmodified dependency. Composing such a tool
reaches the same destination - a multi-agent terminal cockpit - with no fork.
Section 4.6 chooses that multiplexer.

### 4.3 Option B - fork an editor (Zed)

Tempting, because forking Zed appears to deliver code browsing, diffs, and
syntax highlighting for free.

Rejected, for three reasons:

1. Zed is one of the largest and fastest-moving Rust codebases in existence. A
   pinned fork that is continuously rebased onto upstream is a substantial
   ongoing maintenance job by itself - far heavier than a Ghostty fork.
2. PANopt's features would live in the exact subsystem Zed is actively
   iterating on (the agent panel, ACP, terminal threads). Every upstream
   improvement to that subsystem becomes a merge conflict in the fork. The
   fork would be racing the Zed team on their own home turf.
3. It changes the product from "agent coordinator" into "editor with agents,"
   which is a far larger surface to build and maintain.

### 4.4 Decomposing Solo - what is actually worth owning

Solo breaks cleanly into three parts (Section 2):

1. **Wrap agent CLIs in a GUI.** A commodity - a terminal multiplexer manages
   multiple agent CLI sessions as panes and tabs natively.
2. **MCP server for inter-agent coordination.** No existing tool provides this.
3. **Shared todos and notes.** No existing tool provides this.

Parts 2 and 3 are the real, differentiated value. Part 1 is not worth building.

### 4.5 Decision

PANopt is a **standalone coordination daemon. It forks nothing.**

`panoptd` holds all coordination state and speaks MCP. The host - the program
that hosts the agent terminals and PANopt's own UI - is **Zellij**, an
unmodified terminal multiplexer. Because there is no fork, every upstream
improvement to Zellij is a free upgrade rather than a merge conflict.

An earlier draft of this document made Zed the host. That is superseded;
Section 4.6 records why the host is a multiplexer, not an editor.

### 4.6 The host: a terminal multiplexer, not an editor

The first version of this design used Zed as the host - agents as Zed terminal
sessions, todos and notes as files in Zed. Building it surfaced three
problems, none fixable without forking Zed:

1. **The agent surface is not first-class.** Zed runs external agents in its
   agent panel, which is dock-locked: Zed reserves the center for the code
   editor. The agents are a sidebar, not the main event.
2. **Zed cannot be driven from outside.** There is no CLI and no extension API
   to focus a specific pane or tab. A coordination UI that lists agents could
   never *switch to* one.
3. **The extension API is too thin.** A Zed extension cannot render a navigable
   panel; the projected files work but are passive.

The decisive realization: "do not fork" and "the host is an editor" were two
separate decisions, welded together by accident. Keep the first, drop the
second - the host should be the program built for hosting terminals.

**Zellij** is that program:

- A real terminal multiplexer - clean PTY hosting for any number of agent panes
  and tabs, which a non-multiplexer only approximates.
- Scriptable from outside (`zellij action`), so PANopt can drive it.
- An open plugin API (Rust compiled to WASM) - the sanctioned way to add a
  native, navigable panel with no fork.

This is composition, not a fork: Zellij is installed and configured, never
modified. It is categorically different from the Ghostty fork of Section 4.2,
which would have meant owning a terminal emulator's source.

The cost the earlier draft accepted - "no single bespoke window" - is largely
recovered. Agents and PANopt's coordination sidebar are panes in one Zellij
window: a coherent cockpit. It is a terminal-grid UI rather than a GPU-rendered
bespoke application, which for a coordination sidebar is entirely adequate. An
editor (Zed, helix, or any other) is opened on demand to read the projected
files and browse code - a tool reached for, not the frame.

## 5. System Architecture

### 5.1 Components

- **`panoptd`** - the daemon. Rust, long-lived. Holds all state. Exposes an MCP
  server.
- **Projected files** - `.panopt/` markdown files mirroring daemon state into
  the workspace, readable by any editor.
- **PANopt Zellij plugin** - a Rust-to-WASM Zellij plugin. The cockpit runs
  five instances of the same wasm stacked in the left column, keyed off a
  `mode` configuration value (`todos`, `agents`, `terminals`, `commands`,
  `notes`), so each pane renders exactly one kind. Selecting an item
  swaps its pane into the single content slot on the right (Section 5.5). The
  Todos pane doubles as the cockpit gatekeeper, owning the close-gate and
  blank-pane spawn pipes (Section 5.5).
- **Viewer panes** - long-lived `panopt _viewer` panes that display a todo,
  note, or section list, re-pointed by the sidebar through a routing file.
- **Agents** - external CLIs (Claude Code and similar), run as Zellij panes or
  tabs, each configured with the daemon as an MCP server.
- **`panopt`** - the launcher CLI. Starts `panoptd` on demand and opens agent
  panes in Zellij, each with a stable per-agent identity (Section 9). Its
  `todo`, `agent-tool`, and `process` subcommands are small MCP clients for
  editing a project's todos and roster (the two-layer agent_tools / processes
  split, Section 6.6) from a shell; the cockpit's todo form and viewer panes
  are also `panopt` subcommands.

### 5.2 Diagram

```
        Zellij  (terminal multiplexer - the cockpit, one window)
  +-------------------------------------------------------+
  |  five PANopt plugin panes   one content pane          |
  |  (todos / agents /          (viewer / agent / ...)    |
  |   terminals / commands /                              |
  |   notes, stacked)                               |
  +------|--------------------|------|-------------------+
         | reads .panopt/*.md |  MCP |  MCP
         | + focuses panes    | (HTTP)|(HTTP)
         |                    v       v
  +------------------------------------------------------+
  |                   panoptd  (daemon)                  |
  |                                                      |
  |   MCP server  ->  state: todos, notes, locks,  |
  |                   agent registry, agent_tools,       |
  |                   processes                          |
  |                        |                             |
  |                        +-> filesystem projector ---> .panopt/*.md
  |                        +-> SQLite (persistence)      |
  +------------------------------------------------------+

  Any editor (Zed, helix, ...) also opens .panopt/*.md on
  demand, live-reloading. The projection is host-agnostic.
```

There are two planes. The **coordination plane** is agents talking to the
daemon over MCP. The **presentation plane** is the daemon projecting state into
`.panopt/*.md`, surfaced by the Zellij plugin and by any editor.

### 5.3 Transport

The daemon listens on **HTTP/SSE (Streamable HTTP)**, not stdio.

Rationale: stdio MCP is one-client-per-process. If agents used stdio, each
would spawn its own private copy of the daemon with no shared state, which
defeats the entire purpose. A shared coordination hub requires one daemon
serving many connections, which means a network transport. HTTP is chosen
because agent CLIs configure HTTP MCP servers by URL with no friction.

The bind address is configurable (`panoptd --host <addr>`). It defaults to
`127.0.0.1` for single-machine use; setting `0.0.0.0` (or a specific
interface) lets agents on other hosts join the coordination plane - an agent
on a Mac driving Solo, an agent on a host with USB-attached debug hardware,
etc. Every request, loopback or remote, must carry a bearer token via
`Authorization: Bearer <token>` (or `?token=<token>` as a fallback for
clients that cannot set headers). The token is generated by the daemon on
first boot at `<data-dir>/panopt/token` with mode 0600 and shared with local
clients via the filesystem; remote agents copy it over a secure channel.
Always-required auth is a uniform gate rather than a per-host policy: the
token file is owner-readable, so a process able to read it is already in the
same trust domain as the daemon, and a single rule is simpler to reason
about than "loopback unauthenticated, remote authenticated." The user-facing
recipe for the cross-machine case - `panopt up --host 0.0.0.0` on the
daemon host, `panopt token` to extract the token, and `panopt agent-config
--host <addr> --token <value>` on the agent host - lives in the README.

There is one daemon instance, and it serves every project at once. A project
is selected per connection by a `ws` query parameter on the MCP URL:
`http://HOST:PORT/mcp?ws=<absolute project path>&agent=<id>&name=<friendly>&token=<token>`.
An agent's launcher (cockpit-spawned or `panopt agent-config` for
hand-launched) captures that path from `$PWD`, so registration is the moment
the project is named - the daemon never has to guess. The daemon
canonicalizes the path, so symlinks and trailing slashes collapse onto one
project, and distinct git worktrees are distinct projects unless deliberately
pointed at the same path. The URL also carries an `agent` parameter (a
stable per-agent id used as the registry key) and an optional `name`
parameter that the daemon applies as an implicit `identify` on first sight,
so a single URL is enough to land an agent as a first-class citizen
(Sections 6.3 and 9). Every project's state lives in one SQLite database the
daemon owns (Section 6.4).

### 5.4 Coordination plane (agents to daemon)

This plane does not involve the host at all.

Each agent CLI is configured with PANopt as an MCP server in its own
configuration (for example, `claude mcp add --transport http panopt
http://127.0.0.1:PORT/mcp?ws=$PWD`, run inside the project). Zellij is merely
the process that launched the terminal; it is not in the loop.

An agent is registered with the daemon automatically: the first tool call on a
connection adds it to the agent registry (Section 6.3), keyed by its connection
key. The `identify` tool then enriches that entry with a human name and
status, and `whoami` / `agent_list` read it back. An agent gains the
coordination tools - `todo_*`, `note_*`, the registry tools, and `lock_*`
- simply by connecting. Because this plane is pure MCP, it works
identically with any MCP-capable agent and any host - Zellij today, a bare
shell, or anything else later. The coordination core is never welded to the
host.

### 5.5 Presentation plane (daemon to the cockpit)

Two mechanisms surface the daemon's state - one host-agnostic, one
Zellij-native:

1. **Filesystem projection (the backbone).** On every state mutation the daemon
   atomically rewrites markdown files under `.panopt/` (`todos.md`,
   `note/<id>.md`) - write to a temp file, then rename, so a reader never
   sees a half-written file. Any editor with live file-reload renders them as
   ordinary buffers: file-tree entries, syntax highlighting, search, splits,
   pinned tabs. This needs zero host integration and is the reliable backbone
   of the design - it works under Zellij, a bare editor, or anything else.

2. **The Zellij plugin (the cockpit sidebar).** The same Rust-to-WASM plugin
   is instantiated five times in the layout, stacked vertically in the left
   column. Each instance is keyed by a `mode` configuration value
   (`todos`, `agents`, `terminals`, `commands`, `notes`) and renders
   exactly one kind, read from the projected `.panopt/` files and Zellij's
   own live pane state. Zellij treats distinct configurations as distinct
   plugins, so the five panes share code but not state. Each pane carries its
   own keyboard cursor; the mouse and the arrow keys both drive it.

   Selecting an item shows it in one content pane on the right. The cockpit
   starts with the five sidebar panes and a single empty `panopt _viewer`;
   selecting an item swaps its pane into that one slot and suppresses whatever
   was there - a suppressed pane keeps running, hidden, with no stack and no
   title bar. Todos, notes, and section lists all share the one
   re-pointable viewer pane, which the plugin re-points by writing a small
   routing file the viewer polls; an agent, command, or terminal is its own
   pane, swapped in whole. Moving the cursor previews the selected item in the
   slot without taking focus off the sidebar pane; Enter or a click swaps it in
   and focuses it. If the user splits the content pane, a selection swaps into
   whichever pane was focused last before any sidebar pane took focus.

   The Todos pane doubles as the cockpit gatekeeper: it is the only instance
   that handles the close-gate pipe and the `panopt:spawn-blank-pane` pipe,
   delivered with `--plugin-configuration mode=todos` so the other four panes
   (running the same wasm) cannot accidentally answer. The plugin is the
   policy gate for every destructive Zellij action. The cockpit's generated
   Zellij config rewrites the keybinds for `CloseFocus`, `CloseTab`, and
   `Quit` to `Run "zellij" "action" "pipe"` invocations that reach the Todos
   pane instead of acting directly. The plugin then decides: any of the five
   sidebar panes is absolutely un-closeable - the gate has no override there -
   and any other action that would lose active work (an agent or command
   `process` with a live pane, a terminal `process` or ad-hoc shell whose
   foreground command is not the user's shell) opens a floating
   `panopt _close-gate` confirmation dialog that lists the affected items and
   offers a `close anyway` override. On override the plugin invokes the
   matching zellij-tile API call directly, which bypasses the rewritten
   keybinds so the gate is not re-triggered. Outside that, the plugin still
   does not close a pane the user did not explicitly confirm.

   It is still the cockpit's launcher, not an editor: creating and quick-editing
   a todo open `panopt todo edit`, a `ratatui` form, in a floating pane. A WASM
   plugin is Zellij's sanctioned extension point and requires no fork; it
   requests Zellij permissions (`ReadApplicationState`, `ChangeApplicationState`,
   `RunCommands`) once, then is cached.

The earlier draft named "no pixel-native panel" as the one real limitation of
the no-fork decision. The Zellij plugin removes most of it: the sidebar is a
native pane in the cockpit. What remains is only that it is a terminal-grid UI
rather than a GPU-rendered surface - immaterial for a coordination sidebar.

## 6. Data Model

### 6.1 Todos

Fields: id, title, body, status (open / in_progress / backlog / draft /
completed / not_done), priority (high / medium / low), assignee, tags,
blockers, comments, and created / updated / completed timestamps. `draft`
is panopt's own addition for an early note the author has not yet committed
to doing, and `not_done` is panopt's own addition for todos that were
closed without being completed (cancelled, won't-fix); only `completed`
populates the `completed` timestamp. This
mirrors a trimmed subset of
Solo's `todos` table plus its `todo_comments` and `todo_blockers` side tables.
The one deliberate divergence is `assignee`: Solo uses a foreign key to an
agent, but PANopt's registry is in-memory and ephemeral (Section 6.3), so the
assignee is a plain free-text name that cannot dangle.

Projection: one file per todo at `.panopt/todos/<id>.md` - a `---` frontmatter
block of the structured fields above the title, body, and comment thread -
plus a `.panopt/todos.md` index that links them all. Each per-todo file is a
live *view* with lightweight write-back planned: toggling a checkbox or editing
a frontmatter field becomes a change the daemon parses back into an update.
Richer mutations go through the sidebar plugin or MCP tools. The files are a
view first and an input second.

### 6.2 Notes

Append-oriented shared notes, organized into sections and tags.

Projection: one file per note at `.panopt/note/<id>.md`, plus a
`.panopt/notes.md` index that links them all (the cockpit reads the index
to list them). Because notes are append-mostly, bidirectional sync is
clean: a user editing in any editor and an agent appending via MCP both land,
and the daemon reconciles by section. Appends are conflict-free by construction.

These were called *scratchpads* through schema V8; V9 renamed the table and the
`note_*` tool surface (todo #79) because the concept is durable, not ephemeral.
Whether a note should additionally carry a `type` (note / plan / memory /
inter-agent text) is a deferred follow-up - notes are currently untyped.

### 6.3 Agent registry and locks

The registry tracks the agents the daemon has seen, scoped per project. Each
entry is keyed by a connection key and carries a name and a free-form status,
both set through the `identify` tool; `whoami` returns the caller's own entry
and `agent_list` returns every agent on the project. The first tool call on
any connection registers the agent, so even one that never calls `identify`
still appears.

Two kinds of keys cohabit, with different lifetimes:

- A *declared* key is the `agent` query parameter the launcher (or
  `panopt agent-config`) baked into the MCP URL. It names a *person or
  process* - `greg-main`, `backend-a3f7` - and survives every kind of
  reconnect. Declared entries persist until they explicitly leave (the
  `agent_leave` MCP tool) or the daemon restarts; the idle sweep does not
  touch them. A declared identity that has gone quiet stays in the roster
  with an `(idle X)` annotation in `.panopt/agents.md` so a peer can tell at
  a glance that the agent is *known* but *quiet*.
- A *session* key is the rotating `mcp-session-id` header Claude Code mints
  per connection. It really is throwaway - a single Claude Code agent
  produces a stream of unrelated session keys over its lifetime - and the
  idle sweep is what stops them from accumulating. A session-keyed entry
  that has made no tool call for 30 minutes is pruned and its locks are
  released; a background sweep in the daemon runs every 30 seconds so a
  closed agent leaves the registry even when no other agent is active to
  trigger one.

The registry is in-memory only, never persisted: a daemon restart correctly
clears it and lets it refill as agents reconnect. The registry is projected
to `.panopt/agents.md` like every other piece of state, with each line
annotated `(idle X)` so the human-readable file shows presence *and*
staleness rather than presenting an idle declared agent as if it were live.

Cockpit-spawned agent panes get connection-based presence as a bonus: the
sidebar plugin watches Zellij's pane manifest and runs `panopt _agent-leave`
the moment an agent pane closes, so its registry entry and locks clear
immediately instead of waiting on the (never-firing, for declared) idle
sweep. Hand-launched agents that crash without calling `agent_leave` linger
under the `(idle X)` annotation until daemon restart - an accepted edge
case, mitigated by the cockpit hook covering the common path.

Locks are advisory: a lock is a named claim one agent holds so others
coordinate exclusive work voluntarily - the daemon records the holder but
enforces nothing. `lock_acquire` takes a lock and is non-blocking (it reports
the current holder rather than waiting), `lock_release` frees it, and
`lock_status` lists every lock held in the project. Like the registry, locks
are in-memory and ephemeral. A lock is released automatically when its
holder is pruned (session keys), when its holder calls `agent_leave`
(either kind), or when the launcher reports the holder's pane has closed.
The table is projected to `.panopt/locks.md`.

### 6.4 Persistence

A single SQLite database holds every project. One database file is simpler to
operate than a file per project, and SQLite gives durability and queryability
with no server.

Five tables: `projects`, `todos`, `notes`, `agent_tools`, and `processes`
(Section 6.6). The latter four are keyed by `(project_id, id)`, so ids restart
at 1 in each project and read naturally in the projected files. A single
per-project `next_id` counter on the `projects` row is shared across all four
resource types and bumped in the same transaction as the insert, so an id is
never handed out twice, never reused after a deletion, and a `#N` reference
points to exactly one resource. Each mutation commits its transaction and then
re-projects the affected `.panopt/` file; the first time a project is touched
in a daemon run, every file is re-projected from the database, which both
initializes a new project and self-heals a restarted one. The database file
lives in the per-user data directory (`panopt/panopt.db`); the daemon owns it,
and no project ever sees it.

### 6.5 Conflict model

The daemon is the single source of truth. Projected files are derived state.

- Notes: section-based merge; appends never conflict.
- Todos: the projected file is read-mostly. Checkbox toggles are parsed back;
  structural edits go through commands. Where a direct file edit and a daemon
  update collide, the daemon's value wins (last-writer-wins under daemon
  authority).

### 6.6 Agent tools and processes

PANopt splits a project's launchable agents and commands across two tables, a
config layer and an instance layer, mirroring Solo's two-layer model:

- **`agent_tools`** - durable per-project configurations: name, command, cwd,
  a free-form `tool_type`, and an `enabled` flag controlling whether the
  config is offered in spawn UIs. One tool can back many running instances.
- **`processes`** - per-project instances of an agent, command, or terminal:
  `kind` (`agent` / `command` / `terminal`), name, command, cwd, an optional
  `agent_tool_id` back-reference, plus nullable lifecycle columns (`pid`,
  `status`, `agent_state`, `last_seen`) reserved for a future follow-up that
  owns spawn lifecycle.

The split lets a project carry two Claude instances both spawned from agent
tool `#3`, with each instance addressable by its own `#N`. When a process is
spawned from a tool, the tool's `command` and `cwd` are copied into the
process row so post-spawn edits to the tool don't perturb the running
instance. Deleting a tool nulls the `agent_tool_id` back-reference on any
process that referenced it; the live instance keeps running.

Both tables draw ids from the unified per-project `next_id` counter
(Section 6.4), so a `#N` reference still resolves to exactly one row across
todos, notes, agent_tools, and processes.

Whether a process is currently running is *not* stored today: the cockpit
derives it from live Zellij pane state, same as the pre-V6 roster did. The
nullable lifecycle columns turn that into a follow-up rather than a schema
break, once panoptd owns process spawn (Section 10).

The config and instance layers each project to their own markdown file:
`.panopt/agent_tools.md` and `.panopt/processes.md`. The cockpit sidebar
currently renders only `processes.md`, because it is a live-state view;
agent_tools render once a spawn UI lands.

Both layers are distinct from the agent registry (Section 6.3): the registry
is the ephemeral set of MCP agents *currently connected*, while agent_tools
and processes are the durable set of agents/commands/terminals the project
is configured to run and the live instances of them.

## 7. Proof of Concept

Two proofs of concept were built and verified, each retiring the load-bearing
risk of its layer.

### 7.1 POC 1 - the coordination daemon

Goal: prove that multiple agents coordinate through one shared daemon and that
their shared state appears live as files.

Built: `panopt-core` (state and filesystem projection, no protocol
dependencies) and `panoptd` (an MCP server over Streamable HTTP, built on
`rmcp`, `tokio`, and `axum`). In-memory state behind a `Mutex`, one-way
projection - no SQLite yet. Seven tools: `note_create`, `note_list`,
`note_append`, `note_read`, `todo_create`, `todo_list`,
`todo_complete` (`note_create` and `note_list` were added to the
original five: notes are id-keyed, so a create tool mints ids and a list
tool discovers them).

Verified: two agents coordinating through a single daemon over MCP; every
mutation projected atomically to `.panopt/*.md`; and the load-bearing check -
an editor live-reloading a projected file the instant the daemon rewrites it,
confirmed on macOS with Zed, no prompt and no flicker.

### 7.2 POC 2 - the Zellij sidebar plugin

Goal: prove that a Zellij plugin can be PANopt's coordination cockpit sidebar.

Built: `panopt-zellij`, a Rust-to-WASM Zellij plugin, and `panopt.kdl`, a
Zellij layout that places it as a sidebar pane.

Verified, all five claims: it renders as a sidebar pane; receives Zellij's live
pane state; reads the `.panopt/` projection; navigates - pressing Enter focuses
the selected pane; and updates live as todos change.

Findings worth recording: a Zellij plugin is an ordinary binary crate compiled
to `wasm32-wasip1`, not a `cdylib` (the `register_plugin!` macro supplies
`main`). The plugin needs `ReadApplicationState` and `ChangeApplicationState`
permissions, granted once and then cached by Zellij.

### 7.3 Deliberately out of POC scope

The POC left these out: persistence, the agent registry, advisory locks,
bidirectional editing, multi-project support, and the rest of Solo's tool
surface. None carried architectural risk - they are more of the same once the
proven loops exist. Persistence, multi-project support, the agent registry,
advisory locks, and the full todo data model with its editing tools and
per-file projection have since been built (Sections 5.3, 6.4, 6.3, and 6.1);
bidirectional editing and the remaining note and process tools remain.

## 8. Technology Choices

- **Language: Rust.** The MCP SDK (`rmcp`) is Rust; it matches Solo's own
  stack; it produces a single static binary well suited to a long-lived
  daemon.
- **MCP: `rmcp`**, the official Rust SDK, using the Streamable HTTP transport.
- **Async runtime: `tokio`.**
- **HTTP: `axum`** (the basis of `rmcp`'s HTTP transport).
- **Persistence: SQLite** (`rusqlite` or `sqlx`).
- **TUI: `ratatui`.**
- **Zellij plugin: Rust compiled to WASM**, per Zellij's plugin model.

## 9. Risks and Open Questions

- Editor live-reload of the projected files - the load-bearing assumption of
  the presentation plane. Verified in POC 1 on macOS with Zed.
- Zellij plugin-API maturity and version coupling. A plugin is compiled against
  a specific `zellij-tile` version; a Zellij upgrade can require recompiling it.
- Ergonomics of bidirectional todo editing. If write-back proves fiddly, todos
  may degrade gracefully to a read-only projected view plus command-driven
  editing.
- Daemon lifecycle: decided - on-demand launch. The first agent (or the Zellij
  layout) starts `panoptd` if nothing is already listening on its port; one
  global daemon then serves every project. There is no systemd unit or other
  system integration, so the daemon behaves identically on macOS and Linux. The
  `panopt` launcher performs that start-if-absent check, starting `panoptd`
  detached in its own session so it outlives the launching terminal and every
  Zellij session. The daemon also runs a two-strike SIGTERM guard: the first
  shutdown signal is refused if any MCP client is still connected (the daemon
  logs the per-project agent count and keeps serving); a second signal within
  a short confirmation window exits. SIGKILL bypasses this by design.
- Agent identification: solved. Project selection is explicit (the `ws` URL
  parameter, Section 5.3). The MCP session id alone is unreliable as an agent
  key - a Streamable HTTP session is connection-episode-scoped and rotates when
  the client reconnects - so the daemon keys an agent on a stable `agent` query
  parameter when the URL carries one, falling back to the session id otherwise.
  The `panopt` launcher gives each agent pane a unique `agent` id, via a
  per-pane `PANOPT_AGENT` environment variable that the agent's MCP config
  expands into the URL, so an agent keeps one identity across session churn.
  Hand-launched agents (a `claude --mcp-config ...` started outside the
  cockpit, or on another host) are first-class through the same mechanism:
  `panopt agent-config` emits a config with a stable `agent` id, a friendly
  `name` (applied as an implicit identify on first sight), and the bearer
  token, so the daemon sees them indistinguishably from cockpit-spawned
  panes.
- Agent presence: solved. Treat agents as *declared identities* rather than
  activity samples. A stable `?agent=<id>` key persists until an explicit
  `agent_leave` (or daemon restart); only rotating session-id keys idle-
  prune. The presentation plane annotates each agent with `(idle X)` so
  staleness is visible without being acted on. The cockpit launcher closes
  the loop for the OS-level signal it *can* observe: when an agent pane
  dies, the sidebar plugin runs `panopt _agent-leave --id <id>` so the
  registry entry and any held locks clear immediately. Hand-launched agents
  that crash without leaving cleanly are an accepted edge case - their
  entry shows as `(idle X)` until daemon restart, but their locks stay
  associated with a known identity, so the human can see exactly what is
  blocked and on whom. (See todo #83 for the design that supersedes the
  earlier idle-prune-everything model.)

## 10. Out of Scope and Future Work

- The remainder of Solo's large tool surface (timers, prompt templates, process
  spawn and management, services). Added incrementally after the POC.
- Process spawning and supervision. Solo spawns and manages processes itself;
  PANopt initially relies on Zellij to host agent and command panes, and may
  add direct process management later.

## Appendix: Rejected Approaches

- **angr / static binary analysis of Solo** - wrong tool; symbolic execution
  does not scale to a large Rust/Tauri GUI. Use dynamic observation instead.
- **Fork Ghostty** - owning a terminal emulator's source is a permanent
  maintenance cost; an unmodified multiplexer (Zellij) delivers multi-agent
  terminal management by composition instead.
- **Fork Zed** - unmaintainable; forks the exact subsystem Zed is actively
  developing, turning every upstream improvement into a merge conflict.
- **Zed as the host, unforked** - the agent surface is dock-locked, Zed cannot
  be driven from outside, and its extension API cannot render a navigable
  panel. Superseded by Zellij; see Section 4.6.
