# PANopt

A personal coordination daemon for multi-agent workflows. One long-lived
process holds shared todos and scratchpads for every project, tracks which
agents are connected and the advisory locks they hold, persists the todos and
scratchpads in SQLite, exposes everything as an MCP server, and mirrors each
project's state into `.panopt/*.md` files that an editor renders live.

See `DESIGN.md` for the full design rationale. This repository contains the
proof of concept (`DESIGN.md` section 7) plus the persistence, multi-project,
and agent-registry build-out (sections 5.3, 6.4, and 6.3).

## Layout

- `crates/panopt-core` - persistent state (SQLite) and filesystem projection. No
  MCP, async, or HTTP dependency; `cargo tree -p panopt-core` contains none of
  rmcp / axum / tokio.
- `crates/panoptd` - the daemon: an MCP server over Streamable HTTP that wraps
  `panopt-core` in a shared `Arc<Mutex<_>>`.
- `crates/panopt` - the launcher CLI: boots the cockpit and opens agent panes,
  each with a stable per-agent identity.
- `crates/panopt-zellij` - the Zellij sidebar plugin (Rust to WASM): the
  in-cockpit agent/todo list, and the spawner for new agent panes.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
```

## The cockpit

Boot the whole cockpit - the daemon, a Zellij session, the sidebar plugin, and
a first agent - with one command, from the project directory:

```sh
panopt up
```

Re-running `panopt up` in a project whose cockpit already exists just attaches.
Inside the cockpit, add more agents by pressing `a` in the sidebar, or:

```sh
panopt agent [name]
```

Each agent is a Claude pane wired to PANopt with a stable per-agent identity.
The two sections below are what `panopt up` automates - run them by hand to
understand the pieces, or to use PANopt without the launcher.

## Run the daemon

```sh
cargo run -p panoptd -- --port 7600
```

- `--db` (default: `panopt/panopt.db` under the per-user data directory) - the
  single SQLite database holding every project's state. Its parent directory is
  created if missing.
- `--port` (default `7600`) - localhost TCP port. The MCP endpoint is
  `http://127.0.0.1:<port>/mcp`.

One daemon serves every project at once; there is no per-project daemon and no
`--workspace`. The project is chosen per connection (see below).

The daemon is portable Rust and builds anywhere. The Zed live-reload
integration works wherever Zed runs; it was verified on macOS.

## Connect an agent

Run this inside the project directory:

```sh
claude mcp add --transport http panopt "http://127.0.0.1:7600/mcp?ws=$PWD"
```

The `ws` query parameter is the absolute project path; it scopes the connection
to that project, and the daemon writes that project's `.panopt/` projection
(including a `.panopt/.gitignore` of `*`) under it. Any MCP-capable client
works. State is shared live across every agent connected with the same `ws`.

## Tools

| Tool | Arguments | Returns |
|---|---|---|
| `identify` | `name`, optional `status` | `ok` |
| `whoami` | - | own entry `{name, status, idle_seconds, is_self}` |
| `agent_list` | - | JSON array of agent entries |
| `lock_acquire` | `name`, optional `note` | `{acquired, held_by?}` |
| `lock_release` | `name` | `{released, held_by?}` |
| `lock_status` | - | JSON array of lock entries |
| `scratchpad_create` | `title` | new numeric id |
| `scratchpad_list` | - | JSON array of `{id, title}` |
| `scratchpad_append` | `scratchpad_id`, `content` | `ok` |
| `scratchpad_read` | `scratchpad_id` | scratchpad body |
| `todo_create` | `title` | new numeric id |
| `todo_list` | - | JSON array of `{id, title, status}` |
| `todo_complete` | `todo_id` | `ok` |

Connecting already registers an agent; `identify` just adds a name and status.
The registry key is the `agent=` URL parameter (the `panopt` launcher gives each
pane a unique one), or the MCP session id as a fallback when it is absent.

## Projection

Every state mutation atomically rewrites the affected file:

- `.panopt/todos.md` - all todos as a markdown checklist.
- `.panopt/scratchpad/<id>.md` - one file per scratchpad, id in the filename.
- `.panopt/agents.md` - the roster of currently connected agents.
- `.panopt/locks.md` - the advisory locks currently held.

Writes go to a temp file in the same directory, then `rename` over the target,
so a reader never observes a half-written file.

## Not yet implemented

The TUI and bidirectional editing. See `DESIGN.md` sections 9 and 10.
