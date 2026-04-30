//! Thin TUI-side facade over the engine.
//!
//! Slice 4+ consolidation: the engine owns the multi-task graph, the log
//! store, the cancel ladder, and process tracking. The TUI's old
//! `TaskRunner` (which used to manage all of that) is gone; what remains
//! here is just type re-exports for the sidebar/event loop.
//!
//! See `docs/plans/notes/architecture.md` and the dual-path cleanup plan
//! for the full picture.

pub use crate::execution::{ProcessInfo, ProcessStatus, TaskFailure, TaskStatus};
