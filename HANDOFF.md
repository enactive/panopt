# Handoff: cockpit refinement (todo #67)

Source todo: `solo://proj/4/todo/refine-todo--67` ("refine todo").

The cockpit is a Zellij sidebar plugin (left) plus one content pane (right).
Selecting any sidebar item - todo, scratchpad, agent, command, terminal - swaps
its pane into that single content slot, suppressing whatever was there.
Suppressed panes keep running, hidden: no stack, no title bars.

## Status

The workspace builds clean and all 86 tests pass. The plugin compiles to wasm.
The swap-in-place cockpit is implemented but has NOT been driven by a human -
see "Needs verification".

Nothing is committed. Changes sit in the working tree.

## The model

- Cockpit = sidebar plugin + one "main" content slot. Every content pane - the
  doc viewer, each agent, each terminal - is an ordinary Zellij pane; exactly
  one is visible per slot, the rest are suppressed.
- Selecting an item swaps its pane into the designated slot via Zellij's
  `replace_pane_with_existing_pane` (suppressing the displaced pane), or
  `open_command_pane_in_place_of_pane_id` when the pane must be spawned first.
- Documents (todo / scratchpad / list) share one re-pointable `panopt _viewer`
  pane. Agents, commands, and terminals are their own panes.
- Arrowing or clicking selects an item - it shows in the slot, focus staying on
  the sidebar. A row with nothing to show (a section header, a roster entry that
  is not running) clears the slot back to the empty viewer instead. Enter does
  the same and additionally moves focus onto the content pane. Arrowing never
  starts a process; Enter or a click on a stopped roster entry starts it.
- The designated slot = the pane focused last before the sidebar took focus.
  Split the content pane and a selection swaps into the last-touched slot.
- The cockpit starts with the sidebar and one empty doc viewer.

## What changed this iteration

- `panopt-zellij/src/main.rs`: rewritten around the swap-in-place model.
  Content panes are classified from their launch command (`classify_pane`:
  `_viewer` / `_roster-run <id>` / `_agent` / plain shell) read off the pane
  manifest, so the plugin no longer tracks spawn-time pane ids. `show_in_slot`
  and `spawn_in_slot` route every selection through the one slot. The Terminals
  section now lists plain shells only - agents no longer leak into it.
- `panopt/src/up.rs`: the layout is sidebar + one `_viewer --slot main` pane;
  the stacked/tiled swap layouts are gone.
- `DESIGN.md`: section 5.1, the 5.2 diagram, and 5.5 updated to this model.

## Also on this branch (earlier, stable)

The daemon side of the roster: `panopt-core` schema v3 `roster` table plus
`reproject_roster`; `panoptd` `roster_*` MCP tools; the `panopt roster` CLI
subcommand; the `.panopt/roster.md` projection. The cockpit reads `roster.md`.

## How to build and run

```sh
cargo build --workspace
cargo test --workspace

# the Zellij plugin - separate wasm target, excluded from the workspace
cd crates/panopt-zellij && cargo build --release --target wasm32-wasip1
```

The daemon runs detached; `cargo build` does NOT replace a running one. After
any `panoptd` change: `pkill panoptd`, then the next `panopt` call respawns it.

To run the cockpit: `panopt up` in a project directory.

## Needs verification (interactive, not scriptable)

Launch `panopt up` and confirm:

1. The cockpit opens with the sidebar plus one empty viewer ("Select an item").
2. Arrowing or clicking todos/scratchpads/agents/terminals shows them in the
   slot, with focus staying on the sidebar.
3. Arrowing or clicking onto a section header, or a roster entry that is not
   running, clears the slot back to the empty viewer - no stale item lingers.
4. Enter swaps the item in AND moves focus onto the content pane; a click does
   not move focus. `a` spawns a new agent in the slot and focuses it.
5. With the sidebar unfocused, a single click on an item selects and shows it,
   rather than only focusing the sidebar and needing a second click - see the
   note under "Known risks".
6. The displaced pane is suppressed (hidden, still running), not closed, and
   reappears with its state intact when swapped back.
7. Splitting the content pane: a selection swaps into the slot last focused
   before entering the sidebar.

## Known risks / open points

- Whether `replace_pane_with_existing_pane` moves focus is assumed, not
  confirmed. The plugin explicitly refocuses the sidebar (preview/click) or the
  content pane (Enter) after every swap, so it should be correct either way -
  verify there is no focus flicker.
- The first click on the sidebar while it is unfocused: the plugin keeps the
  sidebar focused and selects the row, but only if Zellij delivers that click
  to the plugin. If Zellij instead consumes the first click solely to focus the
  pane, a second click is still needed - that would be a Zellij limitation, not
  fixable from the plugin. Zellij has no config option to change it.
- An exited `_roster-run` pane lingers suppressed; re-activating its entry
  spawns a fresh pane rather than reusing the spent one.
- The viewer still writes routing under `/host/.panopt/.cockpit/`; writes
  degrade gracefully if the plugin cannot write `/host`.
- `panopt _viewer` on `q` closes its own pane; the next document open respawns
  it into the slot.

## Deferred

- In-viewer editing (a key in the viewer to swap to the edit form). Editing is
  the floating `panopt todo edit` form, launched with `c` (new) and `e` (edit
  the focused todo).
- A scratchpad append affordance in the viewer (it is read-only).
- Sweeping stale `viewstate` entries when an item is deleted.

## Key design decisions (the "why")

- One content slot, panes swapped in place. The user wants a single pane that
  renders every kind of item, with arrow-nav previewing without moving focus
  off the sidebar. A live terminal is welded to its pane, so the *pane* is
  swapped, not the process - Zellij's `replace_pane_with_existing_pane` and
  suppressed panes make this exact behavior possible with no terminal
  multiplexer and no pane stack.
- Content panes are re-derived from the Zellij manifest each update
  (`classify_pane` on the launch command), not tracked at spawn time, so the
  plugin's view never drifts from Zellij's. They are sorted by pane id
  (creation order): the manifest's own order is not stable as panes move in and
  out of the suppressed set, which would otherwise reshuffle the sidebar's rows
  under the keyboard cursor.
- Agent rows carry a mutable display label (`agent_labels`, keyed by pane id) -
  today a stable "Agent N" assigned once per agent, never renumbered. Ordering
  is by pane id, never by the label, so the label can later be refreshed from
  each agent's own published activity without ever reshuffling the sidebar. The
  future-facing seam is `agent_labels` plus `sync_agent_labels`: change what
  writes the label, not how rows are ordered.
- Viewer content is always re-read live from the projected `.panopt/` files;
  only view state (scroll/cursor) is persisted, locally, per item.
- The plugin never closes a pane - they hold state the user owns.
