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

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::error::TaskError;
use crate::task::TaskDef;

use super::TaskId;

/// Errors produced by the engine in response to control messages.
///
/// Public because it surfaces through `EngineHandle::*` methods.
///
/// `TaskError` does not currently implement `std::error::Error`, so the
/// `Task` variant carries it without `#[from]`. A future cleanup pass
/// can flip that on once `TaskError` gains the impl.
//
// EngineError carries TaskError, which intentionally does not implement
// Serialize/Deserialize (it conflicts with the blanket `From<T:
// Serialize>` impl). For wire purposes we serialize the `Task` variant as
// a string (its `Display` form) and deserialize it back into a
// stringified TaskError — see the manual serde impls below. The
// supervisor is the only wire consumer today and does not need to
// round-trip the structured `output_json`. If that changes, refactor
// TaskError to expose a serde-friendly snapshot type.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is shutting down")]
    ShuttingDown,
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("{0}")]
    Task(TaskError),
}

// `EngineError` needs `Clone` because it appears (transitively, through
// `RpcError`) inside wire types that derive `Clone`. `TaskError` does
// NOT implement `Clone`, so we implement it manually: clone via the
// Display form. This loses the structured `output_json` of the inner
// `TaskError` but matches the stringified-on-wire behavior already in
// place for serde. See the wire-protocol TODO above.
impl Clone for EngineError {
    fn clone(&self) -> Self {
        match self {
            EngineError::ShuttingDown => EngineError::ShuttingDown,
            EngineError::NotFound(id) => EngineError::NotFound(*id),
            EngineError::Task(err) => EngineError::Task(TaskError::from_display(err.to_string())),
        }
    }
}

// --- Manual serde for EngineError (see TODO above) ---
//
// We serialize as a tagged enum where the `Task` variant carries a string
// (the Display form). On deserialize, we rebuild a `TaskError` via
// `TaskError::from_display`. This loses the structured `output_json` and
// `ExitHint::Code` precision, but is enough for the wire protocol's error
// reporting use case.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum EngineErrorWire {
    ShuttingDown,
    NotFound(TaskId),
    Task(String),
}

impl Serialize for EngineError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            EngineError::ShuttingDown => EngineErrorWire::ShuttingDown,
            EngineError::NotFound(id) => EngineErrorWire::NotFound(*id),
            EngineError::Task(err) => EngineErrorWire::Task(err.to_string()),
        };
        wire.serialize(s)
    }
}

impl<'de> Deserialize<'de> for EngineError {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = EngineErrorWire::deserialize(d)?;
        Ok(match wire {
            EngineErrorWire::ShuttingDown => EngineError::ShuttingDown,
            EngineErrorWire::NotFound(id) => EngineError::NotFound(id),
            EngineErrorWire::Task(msg) => EngineError::Task(TaskError::from_display(msg)),
        })
    }
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
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct SpawnOptions {
    pub timeout: Option<Duration>,
}

/// What kind of cancellation to apply to a task and its subtree.
///
/// Public because it is part of the eventual `EngineHandle::kill_task`
/// surface; the wrapping `Control` message stays `pub(crate)`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum KillSignal {
    /// Run the cancel ladder with `kill_timeout = 2s` (SIGTERM → wait → SIGKILL).
    Term,
    /// Run the cancel ladder with `kill_timeout = 0` (SIGKILL immediately).
    Kill,
}

/// Restart mode passed to `EngineHandle::restart`.
///
/// `Soft` delivers a cooperative signal to the task via its
/// `RestartHandle`. If the task never subscribed to that handle, the
/// engine transparently falls back to `Hard`.
///
/// `Hard` cancels the task subtree and respawns a fresh sibling using
/// the same `TaskDef` and args.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartMode {
    Soft,
    Hard,
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
    /// Restart a top-level task. `Hard` cancels the existing subtree
    /// and spawns a fresh sibling using the same `TaskDef` and args.
    /// `Soft` fires the task's cooperative restart signal if it has
    /// subscribed; otherwise falls back to `Hard`.
    RestartTask {
        id: TaskId,
        mode: RestartMode,
        reply: oneshot::Sender<Result<TaskId, RestartError>>,
    },
}
