# Default recipe: list everything.
default:
    @just --list

# Build the workspace (debug).
build:
    cargo build --workspace

# Build the workspace (release).
release:
    cargo build --workspace --release

# Build the Zellij sidebar plugin (wasm32-wasip1).
plugin:
    cargo build --manifest-path crates/panopt-zellij/Cargo.toml --target wasm32-wasip1

# Build the Zellij sidebar plugin in release mode.
plugin-release:
    cargo build --manifest-path crates/panopt-zellij/Cargo.toml --target wasm32-wasip1 --release

# Run workspace tests.
test:
    cargo test --workspace

# Format all crates (workspace + plugin).
fmt:
    cargo fmt --all
    cargo fmt --manifest-path crates/panopt-zellij/Cargo.toml

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check
    cargo fmt --manifest-path crates/panopt-zellij/Cargo.toml -- --check

# Clippy across the workspace and the plugin, warnings as errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --manifest-path crates/panopt-zellij/Cargo.toml --target wasm32-wasip1 -- -D warnings

# Fast type-check (workspace + plugin).
check:
    cargo check --workspace
    cargo check --manifest-path crates/panopt-zellij/Cargo.toml --target wasm32-wasip1

# Install panopt + panoptd to ~/.cargo/bin.
install:
    cargo install --path crates/panopt
    cargo install --path crates/panoptd

# Run the daemon in the foreground (logs to stderr).
daemon:
    cargo run -p panoptd -- --port 7600

# Tail the daemon log file written by `panopt up`.
logs:
    tail -F ~/.local/share/panopt/panoptd.log

# If a daemon is already running on port 7600, `panopt up` connects to it
# rather than restarting it - use `just stop && just up` to force a fresh
# debug daemon, or `just restart-daemon` to swap the daemon out without
# touching the cockpit.
#
# Launch the cockpit in the current project (debug binaries). `cargo build`
# runs first so `panoptd` reflects the latest sources; without it,
# `cargo run -p panopt` would happily re-spawn yesterday's `panoptd` from
# `target/debug/`.
up:
    cargo build --workspace
    cargo run -p panopt -- up

# Stop the running panoptd (SIGTERM x2 to clear the clients-connected gate).
stop:
    -pkill -TERM -x panoptd
    @sleep 1
    -pkill -TERM -x panoptd

# Rebuild panoptd, stop the running daemon, and re-launch it detached. The
# cockpit's MCP clients reconnect on the next call, so this is the
# ergonomic path while a cockpit is open: edit handler code, `just
# restart-daemon`, the next autosave hits the new binary.
restart-daemon:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -p panoptd
    pkill -TERM -x panoptd 2>/dev/null || true
    sleep 1
    pkill -TERM -x panoptd 2>/dev/null || true
    sleep 1
    mkdir -p "$HOME/.local/share/panopt"
    setsid -f target/debug/panoptd --port 7600 \
        < /dev/null \
        >> "$HOME/.local/share/panopt/panoptd.log" 2>&1
    for _ in $(seq 1 50); do
        if ss -ltn 2>/dev/null | grep -q '127.0.0.1:7600'; then
            echo "panoptd restarted on 127.0.0.1:7600"
            exit 0
        fi
        sleep 0.1
    done
    echo "panoptd did not start listening within 5s; see ~/.local/share/panopt/panoptd.log" >&2
    exit 1

# Open the panopt sqlite database in the sqlite3 shell.
db:
    sqlite3 ~/.local/share/panopt/panopt.db

# Clean both target dirs.
clean:
    cargo clean
    cargo clean --manifest-path crates/panopt-zellij/Cargo.toml

# run a 'connected' agent externally (outside of panopt). this is for connecting a dev-time agent to the built system
devagent:
    claude --mcp-config '{"mcpServers":{"panopt":{"type":"http","url":"http://127.0.0.1:7600/mcp?ws='$PWD'"}}}'

