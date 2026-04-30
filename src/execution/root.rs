//! The synthetic root task.
//!
//! The root anchors the multi-task graph. It is a real `TaskDef` —
//! library-provided, **not** registered via `inventory::submit!` so it
//! never appears in the user-visible task catalog. Its body is a
//! `select` loop over the engine's control channel.
//!
//! Slice 4 wires the real engine: the body reaches `EngineInternals`
//! through `ctx.engine_internals()` (the engine weak set in
//! `spawn_body`), takes the parked `control_rx`, and dispatches the
//! `Control` enum per arch.md §6. `SpawnTask` calls
//! `engine.spawn_child(TaskId::ROOT, ...)`. `KillTask` calls
//! `engine.cancel_subtree_with`. `KillAll` calls `engine.kill_all`.
//! `Quit` exits the loop and the body returns; `cancel_subtree(ROOT)`
//! is invoked on the way out.
//!
//! See `docs/plans/notes/architecture.md` §0, §6.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::{TaskError, TaskResult};
use crate::task::{TaskContext, TaskDef, TaskFnKind};

use super::TaskId;
use super::control::{Control, KillSignal};
use super::engine::take_control_rx;

/// Synthetic root `TaskDef`. Library-provided, never registered with
/// `inventory::submit!` — the root must not appear in the user-visible
/// catalog.
pub(crate) static ROOT_TASK: TaskDef = TaskDef {
    name: "__root",
    description: None,
    group: "__engine",
    func: TaskFnKind::Static(root_body_fn),
    arg_metadata: root_arg_metadata,
    ui_hint: None,
};

fn root_arg_metadata() -> Option<clap::Command> {
    None
}

/// The synthetic root's task body.
///
/// Same signature as any user task body. Reaches `EngineInternals` via
/// `ctx.engine_internals()`, takes the parked `control_rx`, and runs
/// the select loop until `Quit` or the channel closes.
pub(crate) fn root_body_fn<'a>(
    ctx: &'a TaskContext,
    _args: &[String],
) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
    Box::pin(async move {
        let Some(engine) = ctx.engine_internals() else {
            return Err(TaskError::from_display(
                "synthetic root invoked without engine context",
            ));
        };

        let mut control_rx = match take_control_rx(&engine).await {
            Some(rx) => rx,
            None => {
                return Err(TaskError::from_display(
                    "synthetic root invoked but control_rx_slot was already taken",
                ));
            }
        };

        let root_token = ctx.cancellation();

        loop {
            tokio::select! {
                // External shutdown — engine.cancel() fired root's token directly.
                _ = root_token.cancelled() => break,

                msg = control_rx.recv() => {
                    let Some(msg) = msg else { break };  // channel closed
                    match msg {
                        Control::Quit { reply } => {
                            // Reply BEFORE the cancel walk so callers
                            // awaiting quit() don't block on subtree
                            // teardown.
                            let _ = reply.send(Ok(()));
                            break;
                        }

                        Control::SpawnTask { def, args, opts, reply } => {
                            // Engine owns the spawn primitive. Root just
                            // asks the engine to register a child of ROOT
                            // and replies with the id.
                            match engine.spawn_child(TaskId::ROOT, def, args, opts) {
                                Ok(handle) => {
                                    let id = handle.id();
                                    let _ = reply.send(Ok(id));
                                    // Detach: spawn a task that owns
                                    // the handle through completion so
                                    // it doesn't drop-cancel here.
                                    tokio::spawn(async move {
                                        let _ = handle.await;
                                    });
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(
                                        super::control::EngineError::Task(e),
                                    ));
                                }
                            }
                        }

                        Control::KillTask { id, signal, reply } => {
                            let kill_timeout = match signal {
                                KillSignal::Kill => Duration::from_millis(0),
                                KillSignal::Term => Duration::from_secs(2),
                            };
                            let _ = reply.send(Ok(()));
                            let engine_clone = engine.clone();
                            tokio::spawn(async move {
                                engine_clone.cancel_subtree_with(id, kill_timeout).await;
                            });
                        }

                        Control::KillAll { reply } => {
                            let _ = reply.send(Ok(()));
                            let engine_clone = engine.clone();
                            tokio::spawn(async move {
                                engine_clone.kill_all().await;
                            });
                        }
                    }
                }
            }
        }

        // Quit path: walk the graph and cancel everything under root.
        // We pass each direct child to cancel_subtree (rather than the
        // root) so root's own task_handle isn't aborted while we're
        // still in its body.
        let direct_children: Vec<TaskId> = match engine.lookup(TaskId::ROOT) {
            Some(root) => root.children.lock().await.iter().map(|c| c.id).collect(),
            None => Vec::new(),
        };
        for id in direct_children {
            engine.cancel_subtree(id).await;
        }
        Ok(())
    })
}
