//! `TaskBuilder` — lazy configuration value returned by `ctx.run`.
//!
//! Mirrors the shape of `SpawnBuilder` for processes. Nothing is spawned
//! until `.spawn()` is called or the builder is awaited (via
//! `IntoFuture`). Drop-without-spawn is a no-op — the drop-cancels
//! semantics belong to `TaskHandle`, not to the builder.
//!
//! See `docs/plans/notes/architecture.md` §8.

use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::error::{TaskError, TaskResult};
use crate::log::store::LogStore;
use crate::task::{Registry, TaskDef};

use super::TaskId;
use super::control::SpawnOptions;
use super::engine::EngineInternals;
use super::execution::TaskExecution;
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
    /// Parent task id. When the parent is the root or an out-of-engine
    /// caller, this is `None`.
    parent_id: Option<TaskId>,
    /// Weak ref to the parent's `TaskExecution` so children can be
    /// pushed onto its `children` list. `None` for top-level/test paths
    /// where the caller's context has no execution attached.
    parent_exec: Option<Weak<TaskExecution>>,
    /// Weak ref to the engine. When present, `spawn` funnels through
    /// `EngineInternals::spawn_child` (the canonical path). When
    /// absent (out-of-engine test paths), falls back to inline launch.
    engine: Option<Weak<EngineInternals>>,
    /// Shared `LogStore` from the parent's `TaskExecution`. Used only
    /// by the inline-launch fallback when no engine is available.
    log_store: Arc<Mutex<LogStore>>,
    /// Registry to pass to the child's context.
    registry: Arc<Registry>,
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
        parent_exec: Option<Weak<TaskExecution>>,
        engine: Option<Weak<EngineInternals>>,
        log_store: Arc<Mutex<LogStore>>,
        registry: Arc<Registry>,
        task_def: &'static TaskDef,
        args: Vec<String>,
    ) -> Self {
        Self {
            inner: Ok(TaskBuilderInner {
                parent_id,
                parent_exec,
                engine,
                log_store,
                registry,
                task_def,
                args,
                timeout: None,
            }),
        }
    }

    /// Set a per-invocation timeout for the task. The watchdog lives on
    /// `EngineInternals::spawn_child` (slice 4).
    pub fn timeout(mut self, d: Duration) -> Self {
        if let Ok(ref mut inner) = self.inner {
            inner.timeout = Some(d);
        }
        self
    }

    /// Synchronously register and launch the child task. Returns a
    /// [`TaskHandle`] observing the new node.
    ///
    /// When an engine is wired (the production path), funnels through
    /// `EngineInternals::spawn_child` so the child is registered in the
    /// graph table, gets a snapshot publish, and (if a timeout was set)
    /// gets a watchdog. Out-of-engine test paths fall back to a
    /// minimal inline launch that just runs the task body.
    pub fn spawn(self) -> Result<TaskHandle, TaskError> {
        let inner = self.inner?;

        // Production path: route through the engine.
        if let Some(weak) = inner.engine.as_ref()
            && let Some(engine) = weak.upgrade()
        {
            let parent_id = inner.parent_id.unwrap_or(TaskId::ROOT);
            let opts = SpawnOptions {
                timeout: inner.timeout,
            };
            return engine.spawn_child(parent_id, inner.task_def, inner.args, opts);
        }

        // Fallback (no-engine path, used only by tests built via
        // `TaskContext::new` directly): inline launch with no graph
        // registration. The handle still cancels via token-only on
        // Drop. This branch exists to preserve test ergonomics; the
        // real runtime always has an engine.
        let id = TaskId::next();
        let mut exec = TaskExecution::with_log_store(id, inner.log_store.clone());
        exec.set_registry(inner.registry.clone());
        if let Some(parent_id) = inner.parent_id {
            exec.parent = Some(parent_id);
        }
        let task_def = inner.task_def;
        let task_args = inner.args;
        let exec_arc = Arc::new_cyclic(|weak: &Weak<TaskExecution>| {
            let mut e = exec;
            e.launch_with_self_weak(weak.clone(), task_def, task_args);
            e
        });
        if let Some(weak) = inner.parent_exec.as_ref()
            && let Some(parent) = weak.upgrade()
            && let Ok(mut kids) = parent.children.try_lock()
        {
            kids.push(exec_arc.clone());
        }
        let _ = inner.timeout; // inert in fallback path
        Ok(TaskHandle::new(exec_arc, Weak::new()))
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
