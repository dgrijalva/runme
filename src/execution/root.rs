//! The synthetic root task.
//!
//! The root anchors the multi-task graph. It is a real `TaskDef` —
//! library-provided, **not** registered via `inventory::submit!` so it
//! never appears in the user-visible task catalog. Its body is a
//! `select` loop over the engine's control channel.
//!
//! Slice 1 implements the control loop in isolation: `run_root_body`
//! takes a `Receiver<Control>` and a `TaskContext`, and dispatches
//! messages. `SpawnTask` defers to the existing `ctx.run` path (no graph
//! tracking yet). `KillTask` / `KillAll` are stubs. Slice 4 will replace
//! the `ROOT_TASK::func` stub with the real engine wiring so the root
//! body runs as a tokio task launched by `Engine::start`.
//!
//! See `docs/plans/notes/architecture.md` §0, §6.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;

use crate::error::{TaskError, TaskResult};
use crate::task::{TaskContext, TaskDef, TaskFnKind};

use super::TaskId;
use super::control::Control;

/// Synthetic root `TaskDef`. Library-provided, never registered with
/// `inventory::submit!` — the root must not appear in the user-visible
/// catalog.
///
/// The `func` field is a stub that errors if invoked through the regular
/// task plumbing; slice 4 will route the actual root via
/// [`run_root_body`] from inside `Engine::start`.
pub(crate) static ROOT_TASK: TaskDef = TaskDef {
    name: "__root",
    description: None,
    group: "__engine",
    func: TaskFnKind::Static(root_func_stub),
    arg_metadata: root_arg_metadata,
    ui_hint: None,
};

fn root_arg_metadata() -> Option<clap::Command> {
    None
}

/// Slice 1 stub for the root's `TaskFn`. The synthetic root is never
/// dispatched through the registry; if someone reaches this path it is
/// a wiring bug. Slice 4 replaces this with the real launch path inside
/// `Engine::start`.
fn root_func_stub<'a>(
    _ctx: &'a TaskContext,
    _args: &[String],
) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
    Box::pin(async move {
        Err(TaskError::from_display(
            "synthetic root invoked directly; the engine wires it via Engine::start (slice 4)",
        ))
    })
}

/// Run the root task's control loop.
///
/// Returns `Ok(())` when:
/// - a `Control::Quit` message is received, or
/// - the control channel closes (all senders dropped).
///
/// Slice 1 dispatches `SpawnTask` to the existing `ctx.run` path and
/// stubs `KillTask`/`KillAll`. Later slices replace the inline
/// `ctx.run` with `EngineInternals::spawn_child` and wire the cancel
/// ladder to `KillTask`/`KillAll`/`Quit`.
pub(crate) async fn run_root_body(
    mut control_rx: mpsc::UnboundedReceiver<Control>,
    ctx: &TaskContext,
) -> TaskResult {
    while let Some(msg) = control_rx.recv().await {
        match msg {
            Control::Quit { reply } => {
                // Reply BEFORE breaking so callers awaiting `quit()` don't
                // block on subsequent teardown work (which lands in slice 4).
                let _ = reply.send(Ok(()));
                break;
            }

            Control::SpawnTask {
                def,
                args,
                opts: _,
                reply,
            } => {
                // Slice 1: no graph tracking. Run the task inline through
                // the existing single-task path. Resolution is by qualified
                // name so `def` (a registry-resolved `&'static TaskDef`)
                // dispatches deterministically even if short-name lookup
                // would be ambiguous.
                let qualified = qualified_name(def);
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let _ = ctx.run(&qualified, &arg_refs).await;
                // TaskId doesn't carry meaning yet (slice 2 introduces the
                // allocator). Reply with the placeholder `TaskId(0)`.
                let _ = reply.send(Ok(TaskId(0)));
            }

            Control::KillTask {
                id,
                signal: _,
                reply,
            } => {
                tracing::warn!(
                    "Control::KillTask({id}): stub — engine cancel ladder lands in slice 4",
                );
                let _ = reply.send(Ok(()));
            }

            Control::KillAll { reply } => {
                tracing::warn!(
                    "Control::KillAll: stub — engine cancel ladder lands in slice 4",
                );
                let _ = reply.send(Ok(()));
            }
        }
    }

    Ok(())
}

fn qualified_name(def: &TaskDef) -> String {
    if def.group.is_empty() {
        def.name.to_string()
    } else {
        format!("{}:{}", def.group, def.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    use crate::execution::control::SpawnOptions;
    use crate::task::Registry;

    #[tokio::test]
    async fn root_body_quits_on_control_quit() {
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = TaskContext::new("__root");

        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Control::Quit { reply: reply_tx }).unwrap();

        let result = run_root_body(rx, &ctx).await;
        assert!(result.is_ok(), "root body should return Ok on Quit");

        let reply = reply_rx
            .await
            .expect("Quit reply channel should be honored");
        assert!(reply.is_ok(), "Quit reply should be Ok");
    }

    #[tokio::test]
    async fn root_body_exits_when_channel_closes() {
        let (tx, rx) = mpsc::unbounded_channel::<Control>();
        let ctx = TaskContext::new("__root");
        drop(tx);

        let result = run_root_body(rx, &ctx).await;
        assert!(
            result.is_ok(),
            "root body should return Ok when control channel closes",
        );
    }

    #[tokio::test]
    async fn root_body_handles_kill_task_stub() {
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = TaskContext::new("__root");

        let (kill_reply, kill_rx) = oneshot::channel();
        tx.send(Control::KillTask {
            id: TaskId(42),
            signal: crate::execution::control::KillSignal::Term,
            reply: kill_reply,
        })
        .unwrap();

        let (quit_reply, _quit_rx) = oneshot::channel();
        tx.send(Control::Quit { reply: quit_reply }).unwrap();

        let result = run_root_body(rx, &ctx).await;
        assert!(result.is_ok());
        assert!(kill_rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn root_body_handles_kill_all_stub() {
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = TaskContext::new("__root");

        let (kill_reply, kill_rx) = oneshot::channel();
        tx.send(Control::KillAll { reply: kill_reply }).unwrap();

        let (quit_reply, _quit_rx) = oneshot::channel();
        tx.send(Control::Quit { reply: quit_reply }).unwrap();

        let result = run_root_body(rx, &ctx).await;
        assert!(result.is_ok());
        assert!(kill_rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn root_body_dispatches_spawn_task_through_registry() {
        // SpawnTask in slice 1 just runs the task inline through ctx.run.
        // Build a tiny registry with one no-op task and verify the spawn
        // arm replies with a TaskId after dispatching.
        use std::pin::Pin;

        fn noop_task<'a>(
            _ctx: &'a TaskContext,
            _args: &[String],
        ) -> Pin<Box<dyn std::future::Future<Output = TaskResult> + Send + 'a>> {
            Box::pin(async move { Ok(()) })
        }

        fn no_arg_metadata() -> Option<clap::Command> {
            None
        }

        static NOOP: TaskDef = TaskDef {
            name: "noop",
            description: None,
            group: "",
            func: TaskFnKind::Static(noop_task),
            arg_metadata: no_arg_metadata,
            ui_hint: None,
        };

        let mut reg = Registry::new();
        reg.register(&NOOP);
        let reg = Arc::new(reg);

        let mut ctx = TaskContext::new("__root");
        ctx.set_registry(reg);

        let (tx, rx) = mpsc::unbounded_channel();
        let (spawn_reply, spawn_rx) = oneshot::channel();
        tx.send(Control::SpawnTask {
            def: &NOOP,
            args: Vec::new(),
            opts: SpawnOptions::default(),
            reply: spawn_reply,
        })
        .unwrap();
        let (quit_reply, _quit_rx) = oneshot::channel();
        tx.send(Control::Quit { reply: quit_reply }).unwrap();

        let result = run_root_body(rx, &ctx).await;
        assert!(result.is_ok());
        let id = spawn_rx
            .await
            .expect("SpawnTask reply channel should be honored")
            .expect("SpawnTask should succeed in slice 1 stub");
        // Slice 1 placeholder id.
        assert_eq!(id, TaskId(0));
    }
}
