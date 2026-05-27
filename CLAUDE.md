# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workspace layout

Four crates under `crates/`:

- `panopt-core` — transport-agnostic state, persistence (SQLite), filesystem projection. **Must not depend on `rmcp`, `axum`, or `tokio`** — this boundary is the reason core is reusable. Verify with `cargo tree -p panopt-core` if you add a dep.
- `panoptd` — MCP daemon over Streamable HTTP. Defaults to `127.0.0.1:7600`; bind is configurable via `--host` and every request is gated by a bearer token at `~/.local/share/panopt/token` (`Authorization: Bearer <token>` or `?token=<token>`). Wraps `panopt-core` in a `Mutex`.
- `panopt` — CLI launcher and viewer. Starts `panoptd` on demand, drives Zellij, spawns agent panes.
- `panopt-zellij` — Zellij sidebar plugin. **Excluded from the cargo workspace** because it only builds for `wasm32-wasip1`.

## Build / check / fmt / clippy

`just` recipes (`check`, `clippy`, `fmt`, `test`) sweep both the workspace and the plugin. For narrower work, use cargo directly:

- Workspace: `cargo check -p <crate>`, `cargo clippy -p <crate>`
- Plugin: `cargo check --manifest-path crates/panopt-zellij/Cargo.toml --target wasm32-wasip1` (same pattern for `clippy`, `build`, `fmt`)
- Plugin release artifact: `just plugin-release`

Clippy runs with `-D warnings`. Pre-existing warnings in unrelated code are not yours to fix unless asked.

## Daemon lifecycle

`panopt up` does not restart a running daemon — it connects to it. To force a fresh daemon (e.g., after changing daemon code): `just stop && just up`. `just stop` sends `SIGTERM` twice to clear the clients-connected gate.

Logs: `~/.local/share/panopt/panoptd.log` (tail with `just logs`). Database: `~/.local/share/panopt/panopt.db` (open with `just db`).

## Invariants

- **One per-project id counter (`projects.next_id`) is shared across todos, scratchpads, agent tools, and processes**, not derived from `MAX(id)`. A `#N` reference resolves to exactly one resource. Deleting the highest-numbered item must not free that id for reuse. See `db.rs` schema notes.
- **Schema migrations are forward-only** via `PRAGMA user_version`. Add a new `V<n>` block in `db.rs` and an `if version < n` step — do not rewrite earlier versions.
- **The plugin never closes panes**, only suppresses them. Swap-in-place is the model: a suppressed pane keeps running hidden, the user owns its lifecycle.
- **Filesystem projection is atomic**: write to a temp file, then `rename` over the target. Never write directly to `.panopt/*.md`.

## Style

- Doc comments explain *why*, not *what*. Existing code uses long `///` blocks above functions and `//!` at crate top — match that voice.
- Errors: `thiserror` for typed crate errors, `anyhow::Context` (`.context("msg")?`) at call sites.
- Branches/commits are free-form. Topic branches merged into `main` is the recent pattern, but no enforced convention.

## Coordination plane (MCP)

The daemon exposes todos, scratchpads, locks, agent registry, and roster as MCP tools. Each MCP connection is scoped to one project via `?ws=<absolute-path>` on the URL. State mirrors to `.panopt/*.md` on every mutation — those files are read-mostly for humans and the Zellij plugin, not a write surface.

## Hand-launching a first-class agent

Cockpit-spawned panes are first-class automatically (the launcher injects `PANOPT_AGENT`, `PANOPT_NAME`, `PANOPT_TOKEN` into the pane env). For a Claude Code session started outside the cockpit — or on another machine — use `panopt agent-config` so the session lands with a stable id, a friendly display name, and the bearer token in one shot:

```
claude --mcp-config "$(panopt agent-config --name greg-main)"
```

Without this, the session falls back to the rotating `mcp-session-id` HTTP header as its agent key. The key churns on every reconnect, locks held under old keys get reaped by the 30s idle sweep, and other agents see the connection as a series of unreferenceable UUIDs. See `crates/panopt/src/agent_config.rs` for the resolver defaults (id = `$USER-$HOSTNAME`).

## Working a panopt todo

When the user asks you to work on a todo (`do #N`, `start #N`, `work on #N`, etc.), call `mcp__panopt__todo_start` as your first MCP action against that todo *instead of* `todo_get`. It returns the same full detail, flips status to `in_progress`, and claims the `todo:<N>` advisory lock so other agents and the sidebar see the project state correctly. When you finish, call `todo_complete`. If `todo_start` returns `{started: false, held_by: <name>}`, stop and tell the user — another agent already owns it.

Use `todo_get` only when the intent is to *read* a todo (browse, answer a question about it, render it) — not to begin work on it.

## Reference

- `DESIGN.md` — full architecture, decision rationale, data model. Read this before non-trivial changes to the data layer or MCP surface.
- `HANDOFF.md` — current in-flight work and deferred items.
