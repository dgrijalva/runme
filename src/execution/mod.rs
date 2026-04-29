//! Engine layer for the multi-task runtime.
//!
//! `TaskExecution` is the unit of task execution. Slice 2 introduces the
//! recursive node shape (id, parent, children, cancellation) and the
//! engine-owned `LogStore`; later slices add `TaskHandle`, `TaskBuilder`,
//! and the public `Engine` / `EngineHandle` surface.
//!
//! See `docs/03-multi-task-runtime.md` for the design and
//! `docs/plans/notes/architecture.md` for the type-level spec.

pub(crate) mod execution;
mod task_id;

// Slice 1 scaffolding for the multi-task runtime. Most items are
// allow(dead_code) until slices 2-4 wire them in.
#[allow(dead_code)]
pub(crate) mod control;
#[allow(dead_code)]
pub(crate) mod root;

// Slice 3: TaskBuilder (returned by `ctx.run`) and TaskHandle
// (drop-cancels lifetime token).
pub mod builder;
pub mod handle;

pub use builder::TaskBuilder;
pub use execution::{ProcessInfo, ProcessStatus, TaskExecution, TaskFailure, TaskStatus};
pub use handle::TaskHandle;
pub use task_id::TaskId;

#[allow(unused_imports)]
pub(crate) use execution::{start_buffer_forwarder, start_tracing_forwarder};
