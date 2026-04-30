//! `TaskBuilder` — lazy configuration value returned by `ctx.run`.
//!
//! Mirrors the shape of `SpawnBuilder` for processes. Nothing is spawned
//! until `.spawn()` is called or the builder is awaited (via
//! `IntoFuture`). Drop-without-spawn is a no-op — the drop-cancels
//! semantics belong to `TaskHandle`, not to the builder.
//!
//! See `docs/runtime_engine_design.md` § Types — `TaskBuilder`.

use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::Weak;
use std::time::Duration;

use crate::error::{TaskError, TaskResult};
use crate::task::TaskDef;

use super::TaskId;
use super::control::SpawnOptions;
use super::engine::EngineInternals;
use super::handle::TaskHandle;

/// Builder returned by [`TaskContext::run`](crate::task::TaskContext::run).
///
/// Lazy: calling `.timeout(d)` mutates configuration; `.spawn()` registers
/// + launches the task and returns a [`TaskHandle`]; `.await` (via
/// `IntoFuture`) does both and waits for completion.
pub struct TaskBuilder {
    inner: Result<TaskBuilderInner, TaskError>,
}

struct TaskBuilderInner {
    /// Parent task id. `None` when the caller's context has no running
    /// task identity (defaults to root in that case).
    parent_id: Option<TaskId>,
    /// Weak ref to the engine. Spawning requires an engine — there is
    /// no engine-less path. (Out-of-engine `ctx.run` calls fail with
    /// `TaskError::from_display("no engine context")` at `.spawn()`.)
    engine: Weak<EngineInternals>,
    /// Resolved task definition. Resolution happens at `ctx.run` call
    /// time so name errors surface synchronously.
    task_def: &'static TaskDef,
    args: Vec<String>,
    /// Per-invocation timeout. Wired through `SpawnOptions::timeout`.
    timeout: Option<Duration>,
}

impl TaskBuilder {
    /// Construct a builder that fails at `spawn()`/`.await` with the
    /// given error. Used by `TaskContext::run` when the registry is
    /// missing or the task name doesn't resolve.
    pub(crate) fn failed(err: TaskError) -> Self {
        Self { inner: Err(err) }
    }

    /// Construct a configured builder. Pub(crate) — call sites build via
    /// `TaskContext::run`.
    pub(crate) fn new(
        parent_id: Option<TaskId>,
        engine: Weak<EngineInternals>,
        task_def: &'static TaskDef,
        args: Vec<String>,
    ) -> Self {
        Self {
            inner: Ok(TaskBuilderInner {
                parent_id,
                engine,
                task_def,
                args,
                timeout: None,
            }),
        }
    }

    /// Set a per-invocation timeout for the task. The watchdog is wired
    /// in `EngineInternals::spawn_child`.
    pub fn timeout(mut self, d: Duration) -> Self {
        if let Ok(ref mut inner) = self.inner {
            inner.timeout = Some(d);
        }
        self
    }

    /// Synchronously register and launch the child task. Returns a
    /// [`TaskHandle`] observing the new node.
    ///
    /// Always funnels through `EngineInternals::spawn_child`. When the
    /// engine has been dropped (runtime shutting down), returns
    /// `Err("no engine context")`.
    pub fn spawn(self) -> Result<TaskHandle, TaskError> {
        let inner = self.inner?;
        let engine = inner
            .engine
            .upgrade()
            .ok_or_else(|| TaskError::from_display("no engine context"))?;
        let parent_id = inner.parent_id.unwrap_or(TaskId::ROOT);
        let opts = SpawnOptions {
            timeout: inner.timeout,
        };
        engine.spawn_child(parent_id, inner.task_def, inner.args, opts)
    }
}

impl IntoFuture for TaskBuilder {
    type Output = TaskResult;
    type IntoFuture = Pin<Box<dyn Future<Output = TaskResult> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let handle = self.spawn()?;
            handle.await
        })
    }
}
