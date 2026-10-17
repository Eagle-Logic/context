//! The parts of `ctx` that a fuzz target has to link against.
//!
//! **This is not a stable API.** `ctx` ships as a binary; its supported
//! surface is the CLI and the MCP server, both versioned and documented.
//! Treat everything here as subject to change in a patch release.
//!
//! Only the extractors and the data model live here, because only they are
//! self-contained: `extract` depends on `model`, `model` depends on nothing,
//! and neither touches the filesystem or the CLI. The binary's other modules
//! (`mcp`, `query`, `render`, `view`, ...) reach back into `main.rs` for
//! `Format`, `est_tokens` and the budgeted-render helpers, so they stay in the
//! binary rather than dragging that cluster across the boundary for no gain.
//!
//! `main.rs` re-exports these two at its own crate root, so a sibling module's
//! `crate::model::` path resolves to this single copy instead of compiling a
//! second one.

pub mod extract;
pub mod model;
