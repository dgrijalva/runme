//! Internal control protocol for the engine.
//!
//! Frontends do not send `Control` messages directly. The eventual public
//! surface (slice 4) is the method-based `EngineHandle` API; methods
//! serialize into these messages with `oneshot` reply channels. Keeping
//! `Control` `pub(crate)` lets the protocol evolve without breaking
//! frontends.
//!
//! See `docs/runtime_engine_design.md` § Control protocol.

use std::time::Duration;

use tokio::sync::oneshot;

use crate::error::TaskError;
use crate::task::TaskDef;

use super::TaskId;

/// Errors produced by the engine in response to control messages.
///
/// Public because it surfaces through `EngineHandle::*` methods (slice 4).
///
/// `TaskError` does not currently implement `std::error::Error`, so the
/// `Task` variant carries it without `#[from]`. A future cleanup pass
/// can flip that on once `TaskError` gains the impl.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is shutting down")]
    ShuttingDown,
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("{0}")]
    Task(TaskError),
}

impl From<TaskError> for EngineError {
    fn from(err: TaskError) -> Self {
        EngineError::Task(err)
    }
}

/// Errors produced by `Engine::restart`.
#[derive(Debug, thiserror::Error)]
pub enum RestartError {
    #[error("task is not top-level: {0}")]
    NotTopLevel(TaskId),
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("engine is shutting down")]
    ShuttingDown,
    #[error("{0}")]
    Task(TaskError),
}

impl From<TaskError> for RestartError {
    fn from(err: TaskError) -> Self {
        RestartError::Task(err)
    }
}

/// Per-invocation spawn options.
///
/// Designed to grow: future fields (ready_when, env overlay, etc.) land
/// here without churning call sites that only set what they need.
#[derive(Default, Clone)]
pub struct SpawnOptions {
    pub timeout: Option<Duration>,
}

/// What kind of cancellation to apply to a task and its subtree.
///
/// Public because it is part of the eventual `EngineHandle::kill_task`
/// surface; the wrapping `Control` message stays `pub(crate)`.
pub enum KillSignal {
    /// Run the cancel ladder with `kill_timeout = 2s` (SIGTERM → wait → SIGKILL).
    Term,
    /// Run the cancel ladder with `kill_timeout = 0` (SIGKILL immediately).
    Kill,
}

/// Internal command messages handled by the synthetic root task.
///
/// `pub(crate)` — frontends use `EngineHandle` methods (slice 4), which
/// serialize into these messages with oneshot replies.
pub(crate) enum Control {
    /// Spawn a new task as a child of the synthetic root.
    SpawnTask {
        def: &'static TaskDef,
        args: Vec<String>,
        opts: SpawnOptions,
        reply: oneshot::Sender<Result<TaskId, EngineError>>,
    },
    /// Cancel one task (and, in slice 4, its subtree).
    KillTask {
        id: TaskId,
        signal: KillSignal,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    /// Cancel each direct child of root. Root stays alive — "back to zero
    /// state," not shutdown.
    KillAll {
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    /// Cancel the entire root subtree and exit the runtime.
    Quit {
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    /// Restart a top-level task: cancel the existing one (subtree stays
    /// in the graph) and spawn a fresh sibling using the same `TaskDef`
    /// and args.
    RestartTask {
        id: TaskId,
        reply: oneshot::Sender<Result<TaskId, RestartError>>,
    },
}
