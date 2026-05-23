# PANopt

PANopt is the shared brain for a desk of terminal-based coding agents.

One long-running daemon holds the todos, scratchpads, advisory locks, and agent
roster for every project you work in, and surfaces them as a sidebar inside a
[Zellij](https://zellij.dev) cockpit. Agents (Claude Code, Codex, anything that
speaks MCP) connect to the daemon as ordinary MCP clients. They see the same
todos you see in the sidebar, can hand work off to each other through locks
and comments, and write notes into scratchpads that you can read live.

![The PANopt cockpit: sidebar on the left, a scratchpad edit form in the middle, a Claude agent pane on the right](docs/cockpit.png)

The cockpit above is one Zellij session. The leftmost pane is the PANopt
sidebar plugin listing the project's todos, agents, terminals, commands, and
scratchpads. The middle pane is the in-cockpit form for editing a scratchpad.
The right pane is a Claude agent already wired to PANopt.

## Why you might want it

- You run more than one agent at a time and you want them to coordinate
  instead of stepping on each other.
- You want a single place to see what every agent is doing, what it is stuck
  on, and what it has written down, without alt-tabbing through windows.
- You want your todos and notes to live in plain markdown files inside the
  project, not in someone else's cloud.

## Prerequisites

- [Zellij](https://zellij.dev) on your `PATH` (the cockpit is a Zellij
  session).
- An MCP-capable agent CLI you want to drive. The `panopt up` launcher spawns
  [Claude Code](https://docs.claude.com/en/docs/claude-code/overview) panes by
  default; any MCP client works for connecting by hand.
- Rust toolchain to build from source (no prebuilt binaries yet).

Linux is the primary target. macOS works; the screenshot above is macOS.

## Install

```sh
git clone https://github.com/<your-fork>/panopt
cd panopt
cargo install --path crates/panopt
cargo install --path crates/panoptd
```

The first command installs the `panopt` launcher and CLI; the second installs
the `panoptd` daemon. Both end up in `~/.cargo/bin`.

## Quick start

From any project directory:

```sh
panopt up
```

That single command starts the daemon if it is not already running, opens a
Zellij cockpit session for the project, mounts the sidebar plugin, and spawns
a first Claude agent pane wired to PANopt. Re-running `panopt up` in a
project whose cockpit already exists just re-attaches.

Inside the cockpit, the sidebar is the keyboard control surface:

| Key | What it does |
|---|---|
| `Up` / `Down` | Move the cursor over agents, todos, and scratchpads. |
| `Enter` | On an agent: focus its pane. On a todo or scratchpad: open it in the viewer pane. |
| `a` | Spawn a new agent pane. |
| `c` | Create a new todo and open the form on it. |
| `Tab` | Switch the sidebar focus between sections. |
| `Ctrl-C` | Close the current form / viewer. |
| `Ctrl-Q` | Quit the cockpit (blocked while items are open or in progress). |

Todos and scratchpads open in a viewer pane on the right; pressing `Enter`
again or `e` switches the viewer into edit mode (the middle pane in the
screenshot). `Tab` walks between fields, `Ctrl-S` saves, `Ctrl-C` closes.

## What lives where

PANopt keeps two kinds of state:

- A single SQLite database, shared across every project you use PANopt with.
  Default location: `~/.local/share/panopt/panopt.db` on Linux, the equivalent
  Application Support path on macOS. Override with `--db` on `panoptd`.
- A markdown projection inside each project, written to `.panopt/` under the
  project root. The directory contains a `.gitignore` of `*`, so it never
  enters your commits unless you ask it to.

The projection mirrors the live state and is rewritten atomically on every
change:

- `.panopt/todos.md` - a checklist index of every todo.
- `.panopt/todos/<id>.md` - one file per todo, with a frontmatter block of
  structured fields, the body, and the comment thread.
- `.panopt/scratchpad/<id>.md` - one file per scratchpad.
- `.panopt/agents.md` - the agents currently connected.
- `.panopt/locks.md` - the advisory locks currently held.
- `.panopt/roster.md` - the project's persistent agents, commands, and
  terminals (what the cockpit launches and tracks).

The projection is read-only today; edits go through the cockpit forms, the
MCP tools, or the CLI below.

## Driving PANopt from the shell

`panopt` is a small client of the daemon. Everything the cockpit does is also
available as a subcommand, so you can script it or use PANopt without
launching the cockpit at all.

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
panopt todo edit 3                  # interactive form on todo 3
panopt todo edit --new              # form on a fresh todo

panopt roster list                  # agents/commands/terminals
panopt agent [name]                 # spawn another agent pane in the cockpit
```

Each invocation targets the current project (override with `--ws <path>`) and
auto-starts the daemon if needed.

## Connecting your own agents

The daemon speaks the standard Model Context Protocol over Streamable HTTP.
To connect an agent yourself, from inside the project directory:

```sh
claude mcp add --transport http panopt "http://127.0.0.1:7600/mcp?ws=$PWD"
```

The `ws` query parameter is the absolute project path; it scopes the
connection to that project. Any MCP-capable client works the same way - point
it at `http://127.0.0.1:7600/mcp?ws=<absolute-project-path>`. State is shared
live across every agent connected with the same `ws`.

The MCP surface includes `todo_*`, `scratchpad_*`, `lock_*`, `roster_*`, and
the agent registry (`identify`, `whoami`, `agent_list`). The full tool list
is in [DESIGN.md](DESIGN.md).

## Running the daemon by hand

`panopt up` and `panopt todo` both auto-start the daemon. If you would rather
run it yourself:

```sh
panoptd --port 7600
```

One daemon serves every project at once. The MCP endpoint is
`http://127.0.0.1:<port>/mcp`. Useful flags:

- `--db <path>` - override the SQLite database location.
- `--port <n>` - localhost TCP port (default 7600).

## Going deeper

[DESIGN.md](DESIGN.md) is the full design document: why PANopt is a daemon
rather than a fork of a terminal emulator, how the cockpit composes with
Zellij, the crate layout, and the MCP tool reference. Read it if you want to
understand the architecture or contribute.
