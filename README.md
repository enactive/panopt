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
The sidebar lists the agents and the todos with one cursor (Up/Down) over both:

- `a` - spawn a new agent pane.
- `c` - create a todo: opens the form on a blank one.
- `Enter` - on an agent, focuses its pane; on a todo, opens the **todo form**.

The todo form (`panopt todo edit`) opens in a *floating* pane over the cockpit -
a real, roomy editing surface, not the cramped sidebar - and closes when you
quit it. Agents can also be added with `panopt agent [name]`.

The layout is fixed: the sidebar stays pinned full-height on the left, and agent
panes fill the right. New agents stack on the right by default (the focused one
full-size, the rest collapsed to title bars); `Alt-]` / `Alt-[` toggle between
the stacked arrangement and an even tiled grid.

Each agent is a Claude pane wired to PANopt with a stable per-agent identity.
The two sections below are what `panopt up` automates - run them by hand to
understand the pieces, or to use PANopt without the launcher.

## Editing todos from the CLI

`panopt todo` is a small client of the daemon for a project's shared todos from
a shell - it is also what the cockpit's todo form shells out to.

```sh
panopt todo list                    # every todo, one per line
panopt todo get 3                   # one todo in full
panopt todo create "wire the form"  # prints the new id
panopt todo set 3 --status in_progress --priority high --assignee alice
panopt todo set 3 --tags "ui, mcp"  # replaces the whole tag list
panopt todo done 3                  # mark complete
panopt todo rm 3                    # delete
panopt todo block 4 --by 3          # todo 4 is blocked by todo 3
panopt todo comment 3 "started" --as greg
panopt todo edit 3                  # open the interactive form on todo 3
panopt todo edit --new              # the form on a fresh todo
```

The project is the current directory, or `--ws <path>`. Each invocation
auto-starts the daemon if needed and connects as an observer, so it never lands
in the agent roster.

`panopt todo edit` is the form the cockpit launches - a TUI with labeled
fields: Tab moves between them, Left/Right cycle `status` and `priority`, typing
edits `title`/`assignee`/`tags`/`body`, Ctrl-S saves, Esc quits. It runs the
same from a plain shell as it does in the cockpit's floating pane.

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
| `todo_list` | - | JSON array of todo summaries |
| `todo_get` | `todo_id` | one todo in full - body, comments, blockers |
| `todo_update` | `todo_id`, optional `title`/`body`/`status`/`priority`/`assignee`/`tags` | `ok` |
| `todo_complete` | `todo_id` | `ok` |
| `todo_delete` | `todo_id` | `ok` |
| `todo_add_blocker` | `todo_id`, `blocker_id` | `ok` |
| `todo_remove_blocker` | `todo_id`, `blocker_id` | `ok` |
| `todo_comment_add` | `todo_id`, `body` | new comment id |

Connecting already registers an agent; `identify` just adds a name and status.
The registry key is the `agent=` URL parameter (the `panopt` launcher gives each
pane a unique one), or the MCP session id as a fallback when it is absent.

## Projection

Every state mutation atomically rewrites the affected file:

- `.panopt/todos.md` - an index of every todo as a markdown checklist, each
  entry linking to that todo's own file.
- `.panopt/todos/<id>.md` - one file per todo: a `---` frontmatter block of
  structured fields (status, priority, assignee, tags, blockers, timestamps),
  the title and body, then the comment thread.
- `.panopt/scratchpad/<id>.md` - one file per scratchpad, id in the filename.
- `.panopt/agents.md` - the roster of currently connected agents.
- `.panopt/locks.md` - the advisory locks currently held.

Writes go to a temp file in the same directory, then `rename` over the target,
so a reader never observes a half-written file.

## Not yet implemented

Bidirectional editing - parsing a hand-edit of a projected `.panopt/` file back
into the daemon (toggling a checkbox in `.panopt/todos.md`, say). Today the
projection is write-only; edits go through the form, the MCP tools, or
`panopt todo`. See `DESIGN.md` sections 6.5 and 9.
