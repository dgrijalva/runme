//! `TaskHandle` — the lifetime token returned by `ctx.run(name, args).spawn()`.
//!
//! Slice 3 lands the developer-API drop-cancels semantics: dropping a
//! handle without awaiting it cancels the underlying task by signalling
//! its `CancellationToken`. The full cancel ladder (process stop, body
//! abort, status writes) lives on `EngineInternals` and lands in slice 4 —
//! see `docs/plans/notes/architecture.md` §3 / §10 (slicing notes).
//!
//! The handle holds an `Arc<TaskExecution>`, never a strong engine
//! reference. The engine owns its own `Arc` clone, so dropping the
//! handle does not unregister the task from the graph; it only cancels.

use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::{TaskError, TaskResult};

use super::TaskId;
use super::execution::{TaskExecution, TaskStatus};

/// Lifetime token returned by `TaskBuilder::spawn` (and, via `IntoFuture`,
/// by `.await`-ing a `TaskBuilder`).
///
/// - **Awaiting** consumes the underlying `JoinHandle` and yields the
///   task body's `TaskResult`.
/// - **Dropping** without awaiting cancels the task by firing its
///   `CancellationToken`. In slice 3 this is signal-only; slice 4 will
///   upgrade `Drop` to invoke the engine's full cancel ladder via a
///   `Weak<EngineInternals>`.
/// - **Detaching** uses `tokio::spawn(async move { handle.await })` —
///   the spawned future owns the handle, so the parent's stack drop does
///   not cancel the child.
pub struct TaskHandle {
    /// The execution this handle observes. Held via `Arc`; the engine
    /// (slice 4) keeps its own `Arc` in the task table, so dropping the
    /// handle never removes the node from the graph.
    pub(crate) exec: Arc<TaskExecution>,
    /// Cleared by `IntoFuture` so `Drop` becomes a no-op once the future
    /// owns the wait.
    armed: bool,
}

impl TaskHandle {
    pub(crate) fn new(exec: Arc<TaskExecution>) -> Self {
        Self { exec, armed: true }
    }

    /// Identity of the running task.
    pub fn id(&self) -> TaskId {
        self.exec.id
    }

    /// Clone of the task's cancellation token, for callers that want to
    /// pass it elsewhere (e.g. an external watchdog).
    pub fn cancellation(&self) -> CancellationToken {
        self.exec.cancellation.clone()
    }
}

impl IntoFuture for TaskHandle {
    type Output = TaskResult;
    type IntoFuture = Pin<Box<dyn Future<Output = TaskResult> + Send>>;

    fn into_future(mut self) -> Self::IntoFuture {
        // The future owns the wait — disarm Drop so the cancel signal
        // doesn't fire on completion.
        self.armed = false;
        let exec = self.exec.clone();
        Box::pin(async move {
            // Take the JoinHandle. Slice 4's cancel ladder also takes it
            // (and races with us) — whichever path wins resolves the body.
            let join = {
                let mut slot = exec.task_handle.lock().await;
                slot.take()
            };
            match join {
                Some(handle) => match handle.await {
                    Ok(result) => result,
                    Err(join_err) => {
                        if join_err.is_cancelled() {
                            Err(TaskError::cancelled())
                        } else {
                            Err(TaskError::from_display(format!(
                                "task panicked: {join_err}"
                            )))
                        }
                    }
                },
                // Already awaited (or reclaimed by the cancel ladder) —
                // recover terminal state from the node's status.
                None => match &*exec.task_status().lock().await {
                    TaskStatus::Done => Ok(()),
                    TaskStatus::Failed(failure) => {
                        Err(TaskError::from_display(failure.message.clone())
                            .with_code(failure.exit_code))
                    }
                    TaskStatus::Cancelled => Err(TaskError::cancelled()),
                    TaskStatus::Timeout => Err(TaskError::timeout()),
                    TaskStatus::Setup | TaskStatus::Ready => Err(TaskError::from_display(
                        "task handle already consumed",
                    )),
                },
            }
        })
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Slice 3 fallback: signal-only. The full cancel ladder
        // (process stop → body wait → abort → status write) lands on
        // `EngineInternals` in slice 4; once it does, `Drop` will route
        // through `engine.cancel_task(id)`. Tests in slice 3 verify the
        // token fires; slice 4 tests verify the ladder.
        self.exec.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    //! Slice 3 tests: token-fires-on-drop, awaited handle returns Ok,
    //! child appears in parent's children list. The full cancel ladder
    //! (process stop, body wait, abort, status write) ships in slice 4
    //! and has its own tests there.

    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::error::{TaskError, TaskResult};
    use crate::task::{Registry, TaskContext, TaskDef, TaskFnKind};

    fn no_args() -> Option<clap::Command> {
        None
    }

    fn ok_task<'a>(
        _ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }

    fn slow_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            // Stays alive until cancelled; only fails the test if the
            // sleep completes (drop-cancellation didn't fire).
            tokio::select! {
                _ = ctx.cancellation_signal() => Err(TaskError::cancelled()),
                _ = tokio::time::sleep(Duration::from_secs(30)) => Ok(()),
            }
        })
    }

    static OK: TaskDef = TaskDef {
        name: "ok",
        description: None,
        group: "",
        func: TaskFnKind::Static(ok_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    static SLOW: TaskDef = TaskDef {
        name: "slow",
        description: None,
        group: "",
        func: TaskFnKind::Static(slow_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    fn ctx_with_registry(reg: Arc<Registry>) -> TaskContext {
        let mut ctx = TaskContext::new("test-parent");
        ctx.set_registry(reg);
        ctx
    }

    #[tokio::test]
    async fn handle_awaited_returns_ok_for_successful_task() {
        let mut reg = Registry::new();
        reg.register(&OK);
        let reg = Arc::new(reg);

        let ctx = ctx_with_registry(reg);
        let handle = ctx
            .run("ok", &[])
            .spawn()
            .expect("spawn should succeed for resolvable task");

        let result = handle.await;
        assert!(result.is_ok(), "awaited handle should yield Ok: {result:?}");
    }

    #[tokio::test]
    async fn builder_await_runs_to_completion() {
        let mut reg = Registry::new();
        reg.register(&OK);
        let reg = Arc::new(reg);

        let ctx = ctx_with_registry(reg);
        let result = ctx.run("ok", &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn handle_dropped_un_awaited_fires_cancellation_token() {
        let mut reg = Registry::new();
        reg.register(&SLOW);
        let reg = Arc::new(reg);

        let ctx = ctx_with_registry(reg);
        let handle = ctx
            .run("slow", &[])
            .spawn()
            .expect("spawn should succeed for resolvable task");

        let token = handle.cancellation();
        assert!(!token.is_cancelled(), "token must start un-cancelled");

        drop(handle);

        // Token should be tripped synchronously by Drop.
        assert!(
            token.is_cancelled(),
            "dropping an un-awaited TaskHandle must fire its cancellation token (slice 3)"
        );
    }

    #[tokio::test]
    async fn awaited_handle_does_not_fire_cancellation() {
        let mut reg = Registry::new();
        reg.register(&OK);
        let reg = Arc::new(reg);

        let ctx = ctx_with_registry(reg);
        let handle = ctx
            .run("ok", &[])
            .spawn()
            .expect("spawn should succeed");
        let token = handle.cancellation();

        let _ = handle.await;

        // After awaiting, the body completed normally; Drop must NOT
        // have fired the token.
        assert!(
            !token.is_cancelled(),
            "awaited handle's IntoFuture must disarm Drop"
        );
    }

    #[tokio::test]
    async fn child_appears_in_parents_children_list() {
        // Build a parent TaskExecution-backed context (mirrors what
        // `TaskExecution::launch_with_self_weak` does inside the
        // engine), then have its body call `ctx.run` and inspect
        // the parent's children list.
        use crate::execution::{TaskExecution, TaskId};
        use crate::log::store::LogStore;
        use tokio::sync::Mutex;

        fn parent_task<'a>(
            ctx: &'a TaskContext,
            _args: &[String],
        ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
            Box::pin(async move {
                // Use spawn (not await) so the parent's body returns
                // before the child finishes. The child must already
                // be in `parent.children` by the time spawn returns.
                let _handle = ctx.run("ok", &[]).spawn()?;
                // Drop the handle un-awaited — slice 3 only fires the
                // token; the registered child node still lives in
                // parent.children.
                drop(_handle);
                Ok(())
            })
        }

        static PARENT: TaskDef = TaskDef {
            name: "parent",
            description: None,
            group: "",
            func: TaskFnKind::Static(parent_task),
            arg_metadata: no_args,
            ui_hint: None,
        };

        let mut reg = Registry::new();
        reg.register(&OK);
        reg.register(&PARENT);
        let reg = Arc::new(reg);

        let log_store = Arc::new(Mutex::new(LogStore::new()));
        let parent_exec =
            Arc::new_cyclic(|weak: &std::sync::Weak<TaskExecution>| {
                let mut e = TaskExecution::with_log_store(
                    TaskId::next(),
                    log_store.clone(),
                );
                e.set_registry(reg.clone());
                e.launch_with_self_weak(weak.clone(), &PARENT, vec![]);
                e
            });

        // Wait for the parent body to finish. After it returns, the
        // child must be in parent_exec.children.
        let _ = parent_exec.wait().await;

        let kids = parent_exec.children.lock().await;
        assert_eq!(
            kids.len(),
            1,
            "parent should have exactly one child after `ctx.run` was called"
        );
        assert_eq!(kids[0].task_name, "ok", "child node should reflect the spawned task");
        assert_eq!(
            kids[0].parent,
            Some(parent_exec.id),
            "child's parent_id should match the parent's id"
        );
    }

    #[tokio::test]
    async fn timeout_setter_is_inert_in_slice_3() {
        // Slice 3 stores `timeout` on the builder but does not wire a
        // watchdog. This test pins the contract: setting `.timeout`
        // and awaiting should still resolve to Ok for a fast task.
        let mut reg = Registry::new();
        reg.register(&OK);
        let reg = Arc::new(reg);

        let ctx = ctx_with_registry(reg);
        let result = ctx
            .run("ok", &[])
            .timeout(Duration::from_millis(1))
            .await;

        assert!(
            result.is_ok(),
            "slice 3: .timeout() is configuration only; the watchdog lands in slice 4"
        );
    }

    #[tokio::test]
    async fn run_with_unknown_task_returns_error_at_await() {
        let reg = Arc::new(Registry::new());
        let ctx = ctx_with_registry(reg);
        let result = ctx.run("nonexistent", &[]).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "unknown task: nonexistent"
        );
    }

    #[tokio::test]
    async fn run_without_registry_returns_error_at_await() {
        let ctx = TaskContext::new("orphan");
        let result = ctx.run("ok", &[]).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "no registry available"
        );
    }
}

