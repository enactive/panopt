This is early alpha.  You can kinda do useful things with it right now, but it's
lacking a bunch of functionality and is likely to see fundamental changes.

# PANopt

PANopt is the shared brain for a desk of terminal-based coding agents.

One long-running daemon holds todos, scratchpads, advisory locks, agent
tools, and processes for every project you work in, and surfaces them as a
sidebar inside a [Zellij](https://zellij.dev) cockpit. Agents (Claude Code,
Codex, anything that speaks MCP) connect to the daemon as ordinary MCP clients.
They see the same todos you see in the sidebar, can hand work off to each other
through locks and comments, and write notes into scratchpads that you can read
live.

![The PANopt cockpit: five stacked sidebar panes on the left (todos, agents, terminals, commands, scratchpads), a content pane on the right](docs/cockpit.png)

The cockpit above is one Zellij session. The leftmost column is five
vertically stacked sidebar panes - one for todos, agents, terminals, commands,
and scratchpads - each rendered by its own instance of the PANopt plugin. The
right side is the content slot: selecting any sidebar item swaps that item's
pane into the slot, suppressing whatever was there. Agents, terminals, and
commands run as ordinary Zellij panes; the slot just decides which one is
visible.

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
- Rust toolchain to build from source (no prebuilt binaries yet). A `flake.nix`
  is provided if you prefer Nix.

Linux is the primary target. macOS works; the screenshot above is macOS.

## Install

```sh
git clone https://github.com/<your-fork>/panopt
cd panopt
just install        # or: cargo install --path crates/panopt && cargo install --path crates/panoptd
just plugin-release # build the Zellij sidebar plugin (wasm32-wasip1)
```

`just install` puts the `panopt` launcher/CLI and the `panoptd` daemon in
`~/.cargo/bin`. The sidebar plugin builds to a separate `wasm32-wasip1` target
and is loaded by Zellij from the workspace; `panopt up` expects to find it at
`crates/panopt-zellij/target/wasm32-wasip1/release/panopt-zellij.wasm`. The
`just` recipes (`check`, `clippy`, `fmt`, `test`) sweep both the workspace and
the plugin so you do not have to remember the manifest-path/target dance.

## Quick start

From any project directory:

```sh
panopt up
```

That single command starts the daemon if it is not already running, opens a
Zellij cockpit session for the project (named after the project path so two
projects never share a session), mounts the five sidebar plugin panes, and
spawns a first Claude agent pane wired to PANopt. Re-running `panopt up` in a
project whose cockpit already exists just re-attaches.

Inside the cockpit, each sidebar pane is the keyboard control surface for its
own kind of item. Focus the pane (click it or use Zellij's pane-focus binds),
then:

| Key | What it does |
|---|---|
| `Up` / `Down`, `PageUp` / `PageDown`, `Home` / `End` | Move the cursor inside the focused pane. Arrowing previews the item in the content slot without moving focus off the sidebar. |
| `Enter` / left click | Activate the cursor row: swap its pane into the content slot. `Enter` also moves focus onto the content; a click leaves focus on the sidebar. |
| `a` | Spawn a new agent pane. |
| `c` | (Todos pane) Create a new todo and open the form on it. |
| `e` | (Todos pane) Open the form on the focused todo. |
| `n` | (Scratchpads pane) Create a new scratchpad and open the form on it. |
| `L` | Open the index list for this pane's kind in the content slot. |
| `Ctrl-Q` | Quit the cockpit (gated while items are open or in progress; you confirm in a dialog). |

The content slot to the right of the sidebar is single-occupant: every todo,
scratchpad, agent, terminal, and command becomes a Zellij pane that swaps into
that slot when selected. Suppressed panes keep running hidden; the plugin
never closes them. The todo / scratchpad forms run in their own floating pane.
Inside a form: `Tab` cycles fields, `Ctrl-S` saves, `Ctrl-C` closes.

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
- `.panopt/scratchpads.md` - the scratchpad index.
- `.panopt/scratchpad/<id>.md` - one file per scratchpad.
- `.panopt/agents.md` - the agents currently connected.
- `.panopt/locks.md` - the advisory locks currently held.
- `.panopt/agent_tools.md` - the project's durable agent configurations: the
  config layer (name, command, cwd, tool type, enabled).
- `.panopt/processes.md` - the project's per-project process instances: agent,
  command, and terminal entries the cockpit launches and tracks.

`agent_tools` and `processes` are the two-layer roster: one configuration can
back N running instances, and ids are drawn from the same per-project counter
so a `#N` reference resolves to exactly one row across todos, scratchpads,
agent tools, and processes.

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

panopt agent-tool list              # durable agent configs (config layer)
panopt agent-tool add --name claude --command "claude" --tool-type claude
panopt agent-tool enable 7          # offer this tool in spawn UIs
panopt agent-tool disable 7         # hide without deleting

panopt process list                 # agent/command/terminal instances
panopt process add --kind command --name build --command "cargo build"

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

The MCP surface includes `todo_*`, `scratchpad_*`, `lock_*`, `agent_tool_*`,
`process_*`, and the agent registry (`identify`, `whoami`, `agent_list`). The
full tool list is in [DESIGN.md](DESIGN.md).

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
