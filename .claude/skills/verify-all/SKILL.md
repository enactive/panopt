---
name: verify-all
description: Run the full check + clippy sweep across the workspace AND the WASM plugin (panopt-zellij). Use after non-trivial edits, before declaring work done, or when asked to verify the repo builds cleanly. Captures the dual-target dance (workspace builds for host, panopt-zellij builds only for wasm32-wasip1) so you don't have to remember the right manifest-path/target each time.
---

Run these two recipes in sequence and report results:

1. `just check` — `cargo check --workspace` plus `cargo check` on the plugin with `--target wasm32-wasip1`.
2. `just clippy` — same coverage but with `-D warnings`.

If clippy surfaces errors in code you didn't touch, call them out as pre-existing and do not auto-fix unless the user asks. Only fix warnings introduced by the current change.

Report format:
- One line per recipe: PASS or FAIL with a brief summary.
- For failures: include the first error block verbatim (file:line + message) and your read on whether it's from this session's changes or pre-existing.

Skip `just test` unless the user asks — it's slower and the user may want to gate test runs on a clean check/clippy first.
