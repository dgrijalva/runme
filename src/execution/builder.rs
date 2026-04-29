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
    /// Shared `LogStore` from the parent's `TaskExecution`. Slice 4 will
    /// hoist this to the engine; slice 3 inherits the parent's so logs
    /// land in the same store.
    log_store: Arc<Mutex<LogStore>>,
    /// Registry to pass to the child's context.
    registry: Arc<Registry>,
    /// Resolved task definition. Resolution happens at `ctx.run` call
    /// time so name errors surface synchronously.
    task_def: &'static TaskDef,
    args: Vec<String>,
    /// Per-invocation timeout. Inert in slice 3 — flows into
    /// `SpawnOptions::timeout` for the watchdog wired in slice 4.
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
        log_store: Arc<Mutex<LogStore>>,
        registry: Arc<Registry>,
        task_def: &'static TaskDef,
        args: Vec<String>,
    ) -> Self {
        Self {
            inner: Ok(TaskBuilderInner {
                parent_id,
                parent_exec,
                log_store,
                registry,
                task_def,
                args,
                timeout: None,
            }),
        }
    }

    /// Set a per-invocation timeout for the task.
    ///
    /// In slice 3 this only stores the value — the watchdog that fires
    /// it lives on `EngineInternals::spawn_child` and lands in slice 4.
    /// Tests verify the value flows through the builder; slice 4 verifies
    /// the watchdog actually fires.
    pub fn timeout(mut self, d: Duration) -> Self {
        if let Ok(ref mut inner) = self.inner {
            inner.timeout = Some(d);
        }
        self
    }

    /// Synchronously register and launch the child task. Returns a
    /// [`TaskHandle`] observing the new node.
    pub fn spawn(self) -> Result<TaskHandle, TaskError> {
        let inner = self.inner?;

        // Slice 3 has no engine, so spawn_child lives here as a free
        // function. Slice 4 will move the body onto `EngineInternals`
        // and route both `TaskBuilder::spawn` and
        // `EngineSpawnBuilder::spawn` through it.
        let id = TaskId::next();
        let mut exec =
            TaskExecution::with_log_store(id, inner.log_store.clone());
        exec.set_registry(inner.registry.clone());

        // Cancellation token is already minted (independent, via
        // `CancellationToken::new()`) by `with_log_store`. Slice 4 will
        // hand a `Weak<EngineInternals>` to the `TaskContext` so
        // `ctx.run` and `TaskHandle::Drop` can reach the engine.
        if let Some(parent_id) = inner.parent_id {
            exec.parent = Some(parent_id);
        }

        // Use `Arc::new_cyclic` so the freshly-built `TaskExecution`
        // can launch with a `Weak` to itself baked into the
        // `TaskContext`. The weak ref doesn't bump the strong count,
        // so `Arc::get_mut` (used by `launch_with_self_weak`) is fine
        // — but `new_cyclic` inverts the order: launch happens inside
        // the closure, before the Arc is fully constructed. The
        // closure receives a `&Weak<Self>` that becomes valid once
        // construction completes.
        let task_def = inner.task_def;
        let task_args = inner.args;
        let exec_arc = Arc::new_cyclic(|weak: &Weak<TaskExecution>| {
            let mut e = exec;
            e.launch_with_self_weak(weak.clone(), task_def, task_args);
            e
        });

        // Now safe to clone for the parent's children list and the
        // handle. Use `try_lock` to keep `spawn()` synchronous; in
        // slice 3 nothing else touches `children`, so contention is
        // impossible. Slice 4 will revisit if graph-snapshot reads
        // start to race.
        if let Some(weak) = inner.parent_exec.as_ref()
            && let Some(parent) = weak.upgrade()
            && let Ok(mut kids) = parent.children.try_lock()
        {
            kids.push(exec_arc.clone());
        }

        // The timeout is stored but not consumed in slice 3 — keep the
        // local alive for completeness so future readers see the wiring.
        let _ = inner.timeout;

        Ok(TaskHandle::new(exec_arc))
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
