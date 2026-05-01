//! Engine layer for the multi-task runtime.
//!
//! Slice 4 lands the public engine surface: `Engine::start(registry)`
//! returns an `EngineHandle` which the headless CLI, the TUI, and
//! future MCP frontends consume. The synthetic root, the cancel ladder,
//! per-task timeouts, and the graph snapshot all live here.
//!
//! See `docs/runtime_engine_design.md` for the canonical engine reference.

pub(crate) mod control;
pub(crate) mod engine;
#[allow(clippy::module_inception)]
pub(crate) mod execution;
mod root;
mod task_id;

pub mod builder;
pub mod handle;

pub use builder::TaskBuilder;
pub use control::{EngineError, KillSignal, RestartError, SpawnOptions};
pub use engine::{
    Engine, EngineHandle, EngineSpawnBuilder, GraphSnapshot, ProcessNodeInfo, TaskNode,
};
pub use execution::{ProcessInfo, ProcessStatus, TaskExecution, TaskFailure, TaskStatus};
pub use handle::TaskHandle;
pub use task_id::TaskId;

#[allow(unused_imports)]
pub(crate) use execution::{start_buffer_forwarder, start_tracing_forwarder};
