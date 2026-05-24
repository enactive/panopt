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
# debug daemon.
#
# Launch the cockpit in the current project (debug binaries).
up:
    cargo run -p panopt -- up

# Stop the running panoptd (SIGTERM x2 to clear the clients-connected gate).
stop:
    -pkill -TERM -x panoptd
    @sleep 1
    -pkill -TERM -x panoptd

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

