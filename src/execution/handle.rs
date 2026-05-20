//! `TaskHandle` — the lifetime token returned by `ctx.run(name, args).spawn()`.
//!
//! Slice 3 lands the developer-API drop-cancels semantics: dropping a
//! handle without awaiting it cancels the underlying task by signalling
//! its `CancellationToken`. The full cancel ladder (process stop, body
//! abort, status writes) lives on `EngineInternals` —
//! see `docs/runtime_engine_design.md` § Types — `TaskHandle` and § Cancellation model.
//!
//! The handle holds an `Arc<TaskExecution>`, never a strong engine
//! reference. The engine owns its own `Arc` clone, so dropping the
//! handle does not unregister the task from the graph; it only cancels.

use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use tokio_util::sync::CancellationToken;

use crate::error::{TaskError, TaskResult};

use super::TaskId;
use super::engine::EngineInternals;
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
    /// keeps its own `Arc` in the task table, so dropping the handle
    /// never removes the node from the graph.
    pub(crate) exec: Arc<TaskExecution>,
    /// Weak ref to the engine. When present, `Drop` invokes the full
    /// cancel ladder via `engine.cancel_task(id)`; when absent (engine
    /// gone or out-of-engine test path), `Drop` falls back to a
    /// signal-only `cancellation.cancel()`.
    engine: Weak<EngineInternals>,
    /// Cleared by `IntoFuture` so `Drop` becomes a no-op once the future
    /// owns the wait.
    armed: bool,
}

impl TaskHandle {
    pub(crate) fn new(exec: Arc<TaskExecution>, engine: Weak<EngineInternals>) -> Self {
        Self {
            exec,
            engine,
            armed: true,
        }
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

    /// Wait for the task to transition to [`TaskStatus::Ready`].
    ///
    /// Resolves `Ok(())` when the task body calls `ctx.mark_ready()` or
    /// `ctx.bind_ready(&proc)` flips the status to `Ready`.
    ///
    /// If the task reaches a terminal status without ever becoming
    /// ready, returns an error reflecting that terminal state — `Done`
    /// surfaces as "task completed before reaching ready state" so a
    /// parent waiting on a long-running child doesn't silently block
    /// forever when the child exits early.
    ///
    /// Observation is via the engine's graph snapshot watch, so the
    /// future is wake-driven, not polling.
    pub async fn wait_ready(&self) -> TaskResult {
        let id = self.exec.id;
        let Some(engine) = self.engine.upgrade() else {
            return Err(TaskError::from_display("engine unavailable"));
        };
        let mut rx = engine.graph_tx.subscribe();
        let settled = rx
            .wait_for(|snap| match snap.tasks.get(&id).map(|n| &n.status) {
                Some(TaskStatus::Ready)
                | Some(TaskStatus::Done)
                | Some(TaskStatus::Failed(_))
                | Some(TaskStatus::Cancelled)
                | Some(TaskStatus::Timeout) => true,
                Some(TaskStatus::Setup) | None => false,
            })
            .await;
        match settled {
            Ok(snap_ref) => match snap_ref.tasks.get(&id).map(|n| n.status.clone()) {
                Some(TaskStatus::Ready) => Ok(()),
                Some(TaskStatus::Done) => Err(TaskError::from_display(
                    "task completed before reaching ready state",
                )),
                Some(TaskStatus::Failed(failure)) => {
                    Err(TaskError::from_display(failure.message).with_code(failure.exit_code))
                }
                Some(TaskStatus::Cancelled) => Err(TaskError::cancelled()),
                Some(TaskStatus::Timeout) => Err(TaskError::timeout()),
                Some(TaskStatus::Setup) | None => {
                    Err(TaskError::from_display("wait_for predicate violated"))
                }
            },
            Err(_) => Err(TaskError::from_display("engine dropped graph channel")),
        }
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
                    TaskStatus::Setup | TaskStatus::Ready => {
                        Err(TaskError::from_display("task handle already consumed"))
                    }
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
        // Slice 4: route through the engine's cancel ladder so the
        // status transitions to `Cancelled` and the body's process
        // groups get torn down. Engine gone (out-of-engine test path
        // or runtime shutting down) falls back to a signal-only token
        // cancel.
        let id = self.exec.id;
        if let Some(engine) = self.engine.upgrade() {
            tokio::spawn(async move {
                engine.cancel_task(id).await;
            });
        } else {
            self.exec.cancellation.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    //! `TaskHandle` tests, all driven through a real `Engine` so they
    //! exercise the production path end-to-end. Engine-less paths no
    //! longer exist (item 1 of the dual-path cleanup).

    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::error::{TaskError, TaskResult};
    use crate::execution::{Engine, TaskId, TaskStatus};
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
            tokio::select! {
                _ = ctx.cancellation_signal() => Err(TaskError::cancelled()),
                _ = tokio::time::sleep(Duration::from_secs(30)) => Ok(()),
            }
        })
    }

    fn ready_then_block_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            ctx.mark_ready();
            tokio::select! {
                _ = ctx.cancellation_signal() => Err(TaskError::cancelled()),
                _ = tokio::time::sleep(Duration::from_secs(30)) => Ok(()),
            }
        })
    }

    fn fail_task<'a>(
        _ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move { Err(TaskError::from_display("boom").with_code(7)) })
    }

    fn parent_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            // Spawn (not await) so the parent's body returns before the
            // child finishes. The child must already be in
            // `parent.children` by the time spawn returns.
            let h = ctx.run("ok", &[]).spawn()?;
            drop(h);
            Ok(())
        })
    }

    static OK: TaskDef = TaskDef {
        name: "ok",
        description: None,
        group: "",
        dir: "",
        func: TaskFnKind::Static(ok_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    static SLOW: TaskDef = TaskDef {
        name: "slow",
        description: None,
        group: "",
        dir: "",
        func: TaskFnKind::Static(slow_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    static PARENT: TaskDef = TaskDef {
        name: "parent",
        description: None,
        group: "",
        dir: "",
        func: TaskFnKind::Static(parent_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    static READY_THEN_BLOCK: TaskDef = TaskDef {
        name: "ready_then_block",
        description: None,
        group: "",
        dir: "",
        func: TaskFnKind::Static(ready_then_block_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    static FAIL: TaskDef = TaskDef {
        name: "fail",
        description: None,
        group: "",
        dir: "",
        func: TaskFnKind::Static(fail_task),
        arg_metadata: no_args,
        ui_hint: None,
    };

    fn build_registry(extras: &[&'static TaskDef]) -> Arc<Registry> {
        let mut r = Registry::new();
        for d in extras {
            r.register(d);
        }
        Arc::new(r)
    }

    async fn wait_terminal(handle: &crate::execution::EngineHandle, id: TaskId) -> TaskStatus {
        let mut graph = handle.graph.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snap = graph.borrow().clone();
            if let Some(node) = snap.tasks.get(&id) {
                match &node.status {
                    TaskStatus::Done
                    | TaskStatus::Failed(_)
                    | TaskStatus::Cancelled
                    | TaskStatus::Timeout => return node.status.clone(),
                    _ => {}
                }
            }
            tokio::select! {
                _ = graph.changed() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("task {id} did not reach terminal status before deadline");
                }
            }
        }
    }

    #[tokio::test]
    async fn engine_spawn_task_returns_id_and_completes() {
        let registry = build_registry(&[&OK]);
        let (engine, handle) = Engine::start(registry);
        let id = handle.spawn_task(&OK, vec![]).await.expect("spawn ok");
        let status = wait_terminal(&handle, id).await;
        assert!(matches!(status, TaskStatus::Done));
        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn child_appears_in_parents_children_list() {
        // Parent body uses ctx.run("ok"). After completion, the engine's
        // graph snapshot should show "ok" as a child of "parent".
        let registry = build_registry(&[&OK, &PARENT]);
        let (engine, handle) = Engine::start(registry);
        let parent_id = handle
            .spawn_task(&PARENT, vec![])
            .await
            .expect("spawn parent");
        let _ = wait_terminal(&handle, parent_id).await;
        // Brief sleep so the child snapshot publish settles.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = handle.graph.borrow().clone();
        let parent_node = snap.tasks.get(&parent_id).expect("parent in graph");
        assert_eq!(parent_node.children.len(), 1);
        let child_id = parent_node.children[0];
        let child_node = snap.tasks.get(&child_id).expect("child in graph");
        assert_eq!(child_node.name, "ok");
        assert_eq!(child_node.parent, Some(parent_id));
        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn dropped_handle_fires_cancellation() {
        // Spawn a SLOW task at the engine level, build a TaskHandle
        // pointing at it, and drop it. The cancel ladder should fire
        // the token (step 1) almost immediately.
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);
        let id = handle.spawn_task(&SLOW, vec![]).await.expect("spawn slow");
        let exec = handle.lookup(id).expect("exec in table");
        let token = exec.cancellation.clone();
        assert!(!token.is_cancelled());

        let internals_weak = std::sync::Arc::downgrade(&handle.internals);
        let h = super::TaskHandle::new(exec.clone(), internals_weak);
        drop(h);

        for _ in 0..100 {
            if token.is_cancelled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(token.is_cancelled(), "Drop should fire cancellation token");

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn run_unknown_task_errors_at_await() {
        let registry = build_registry(&[]);
        let (engine, handle) = Engine::start(registry.clone());
        let mut ctx = TaskContext::new("orphan");
        ctx.set_registry(registry.clone());
        let result = ctx.run("nonexistent", &[]).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "unknown task: nonexistent");
        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn run_without_engine_errors_at_await() {
        let mut reg = Registry::new();
        reg.register(&OK);
        let mut ctx = TaskContext::new("orphan");
        ctx.set_registry(Arc::new(reg));
        let result = ctx.run("ok", &[]).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "no engine context");
    }

    #[tokio::test]
    async fn wait_ready_resolves_when_task_marks_ready() {
        let registry = build_registry(&[&READY_THEN_BLOCK]);
        let (engine, handle) = Engine::start(registry);
        let id = handle
            .spawn_task(&READY_THEN_BLOCK, vec![])
            .await
            .expect("spawn ready_then_block");
        let exec = handle.lookup(id).expect("exec in table");
        let internals_weak = std::sync::Arc::downgrade(&handle.internals);
        let h = super::TaskHandle::new(exec, internals_weak);

        let outcome = tokio::time::timeout(Duration::from_secs(5), h.wait_ready())
            .await
            .expect("wait_ready should resolve before deadline");
        assert!(outcome.is_ok(), "wait_ready returned err: {outcome:?}");

        // Dropping the handle cancels the still-blocking child.
        drop(h);
        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn wait_ready_errors_when_task_completes_without_ready() {
        let registry = build_registry(&[&OK]);
        let (engine, handle) = Engine::start(registry);
        let id = handle.spawn_task(&OK, vec![]).await.expect("spawn ok");
        let exec = handle.lookup(id).expect("exec in table");
        let internals_weak = std::sync::Arc::downgrade(&handle.internals);
        let h = super::TaskHandle::new(exec, internals_weak);

        let outcome = tokio::time::timeout(Duration::from_secs(5), h.wait_ready())
            .await
            .expect("wait_ready should resolve before deadline");
        let err = outcome.expect_err("ok task never marks ready");
        assert!(
            err.to_string().contains("completed before reaching ready"),
            "unexpected error message: {err}"
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn wait_ready_propagates_task_failure() {
        let registry = build_registry(&[&FAIL]);
        let (engine, handle) = Engine::start(registry);
        let id = handle.spawn_task(&FAIL, vec![]).await.expect("spawn fail");
        let exec = handle.lookup(id).expect("exec in table");
        let internals_weak = std::sync::Arc::downgrade(&handle.internals);
        let h = super::TaskHandle::new(exec, internals_weak);

        let outcome = tokio::time::timeout(Duration::from_secs(5), h.wait_ready())
            .await
            .expect("wait_ready should resolve before deadline");
        let err = outcome.expect_err("fail task surfaces error");
        assert!(err.to_string().contains("boom"), "unexpected error: {err}");

        let _ = handle.quit().await;
        engine.shutdown().await;
    }
}
