//! `panopt-core` - the runtime- and transport-agnostic core of PANopt.
//!
//! Holds the shared todo and scratchpad state for every project in one SQLite
//! database, tracks connected agents and their advisory locks in memory, and
//! projects each project's state to markdown files on disk. It is deliberately
//! free of any MCP, async, or HTTP dependency so a different front-end (a TUI,
//! a future stdio transport) can drive the exact same state with correct
//! persistence and projection for free.

pub mod auth;
mod db;
mod error;
mod locks;
mod model;
mod projection;
mod registry;
mod state;

pub use error::CoreError;
pub use model::{
    Agent, AgentTool, AgentToolPatch, KeySource, Lock, Priority, Process, ProcessKind,
    ProcessPatch, ProjectId, Scratchpad, ScratchpadPatch, Todo, TodoComment, TodoPatch, TodoStatus,
};
pub use state::Store;
