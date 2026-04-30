//! The engine: synthetic root, task table, control loop, cancel ladder.
//!
//! Slice 4 lands the public surface — `Engine`, `EngineHandle`,
//! `EngineSpawnBuilder`, `GraphSnapshot`, and the cancel ladder
//! (`cancel_task`, `cancel_subtree`, `kill_all`, `timeout_task`).
//!
//! See `docs/plans/notes/architecture.md` §5–§11.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::error::{TaskError, TaskResult};
use crate::log::LogEntry;
use crate::log::store::LogStore;
use crate::task::{Registry, TaskDef};

use super::TaskId;
use super::control::{Control, EngineError, KillSignal, SpawnOptions};
use super::execution::{ProcessInfo, TaskExecution, TaskStatus};
use super::handle::TaskHandle;
use super::root::ROOT_TASK;

/// Cancel ladder timeout — used as the per-step grace period in
/// `cancel_task_with` and as the default `kill_timeout`.
pub(crate) const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// A single immutable snapshot of the task graph.
///
/// `tasks` is wrapped in `Arc<HashMap>` so clones are cheap. New
/// snapshots are produced on every task lifecycle event (spawn, status
/// change, process appeared, readiness flipped, process exited, cancel
/// ladder finished).
#[derive(Clone)]
pub struct GraphSnapshot {
    pub root: TaskId,
    pub tasks: Arc<HashMap<TaskId, TaskNode>>,
}

impl Default for GraphSnapshot {
    fn default() -> Self {
        Self {
            root: TaskId::ROOT,
            tasks: Arc::new(HashMap::new()),
        }
    }
}

/// Snapshot of a single task in the graph.
#[derive(Clone)]
pub struct TaskNode {
    pub id: TaskId,
    pub name: String,
    pub parent: Option<TaskId>,
    pub children: Vec<TaskId>,
    pub status: TaskStatus,
    /// Processes currently owned by this task. Cloned at snapshot time —
    /// `ProcessInfo` is not `Clone` directly because it owns an
    /// `Arc<Mutex<OutputBuffer>>`; we copy by hand.
    pub processes: Vec<ProcessNodeInfo>,
}

/// Snapshot-friendly view of a `ProcessInfo`. The buffer Arc is included
/// so consumers can subscribe; the rest is the displayable lifecycle
/// state.
#[derive(Clone)]
pub struct ProcessNodeInfo {
    pub task_name: String,
    pub command_label: String,
    pub pid: Option<u32>,
    pub pgid: Option<i32>,
    pub status: super::execution::ProcessStatus,
    pub ready: bool,
}

impl ProcessNodeInfo {
    fn from_process(info: &ProcessInfo) -> Self {
        Self {
            task_name: info.task_name.clone(),
            command_label: info.command_label.clone(),
            pid: info.pid,
            pgid: info.pgid,
            status: info.status.clone(),
            ready: info.ready,
        }
    }
}

/// Internal engine state. `pub(crate)` — frontends interact through
/// `EngineHandle`'s methods (which serialize into Control messages).
pub struct EngineInternals {
    pub root: Arc<TaskExecution>,
    pub table: Mutex<HashMap<TaskId, Arc<TaskExecution>>>,
    pub graph_tx: watch::Sender<GraphSnapshot>,
    pub log_store: Arc<Mutex<LogStore>>,
    pub(crate) control_tx: mpsc::UnboundedSender<Control>,
    /// Receiver slot for the synthetic root's control loop. Set during
    /// `Engine::start`; the root body takes it on first run.
    pub(crate) control_rx_slot: Mutex<Option<mpsc::UnboundedReceiver<Control>>>,
    pub registry: Arc<Registry>,
    /// Engine-canonical tracing-installed flag. Shared with each
    /// `TaskExecution` via the existing `set_tracing_installed` setter
    /// at spawn_body time.
    pub tracing_installed: Arc<AtomicBool>,
}

impl EngineInternals {
    /// Look up a task by id. Includes the synthetic root at `TaskId::ROOT`.
    pub async fn lookup(&self, id: TaskId) -> Option<Arc<TaskExecution>> {
        self.table.lock().await.get(&id).cloned()
    }

    /// Walk `table` and broadcast a fresh `GraphSnapshot`. Best-effort —
    /// uses async locks but never blocks longer than briefly.
    pub async fn publish_snapshot(&self) {
        let table = self.table.lock().await;
        let mut tasks = HashMap::with_capacity(table.len());
        for (id, exec) in table.iter() {
            let status = exec.task_status().lock().await.clone();
            let kids: Vec<TaskId> = exec
                .children
                .lock()
                .await
                .iter()
                .map(|c| c.id)
                .collect();
            let procs: Vec<ProcessNodeInfo> = exec
                .processes()
                .lock()
                .await
                .iter()
                .map(ProcessNodeInfo::from_process)
                .collect();
            tasks.insert(
                *id,
                TaskNode {
                    id: *id,
                    name: exec.task_name.clone(),
                    parent: exec.parent,
                    children: kids,
                    status,
                    processes: procs,
                },
            );
        }
        let snapshot = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks),
        };
        // `send` only fails if there are no receivers; the engine itself
        // holds one in `EngineHandle::graph`, so this is effectively
        // infallible during normal operation.
        let _ = self.graph_tx.send(snapshot);
    }

    /// Register and launch a child of `parent_id` running task `def` with
    /// `args` and per-invocation `opts`. Returns the handle synchronously
    /// after the body's tokio task is spawned. Sets up the timeout
    /// watchdog if `opts.timeout.is_some()` (arch.md §11).
    pub fn spawn_child(
        self: &Arc<Self>,
        parent_id: TaskId,
        def: &'static TaskDef,
        args: Vec<String>,
        opts: SpawnOptions,
    ) -> Result<TaskHandle, TaskError> {
        let id = TaskId::next();

        // Build the new execution. Engine-aware constructor wires the
        // monitor_spawns loop with our weak so process events publish
        // snapshots automatically.
        let log_store = self.log_store.clone();
        let engine_weak = Arc::downgrade(self);

        // Prepare the registry on the exec before launch (mirrors
        // legacy launch).
        let new_exec = {
            let mut e = TaskExecution::with_log_store_and_engine(
                id,
                log_store,
                engine_weak.clone(),
            );
            e.parent = Some(parent_id);
            e.set_registry(self.registry.clone());
            // Engine-canonical tracing flag: share with this exec via
            // the existing setter. Only one global subscriber install
            // ever happens because the AtomicBool is shared.
            e.set_tracing_installed(self.tracing_installed.clone());
            e
        };

        // `Arc::new_cyclic` so the body can hold a `Weak<TaskExecution>`
        // pointing at the freshly-built node (for `ctx.run` → child
        // attachment).
        let exec_arc = Arc::new_cyclic(|self_weak| {
            let mut e = new_exec;
            e.spawn_body(self_weak.clone(), engine_weak.clone(), def, args);
            e
        });

        // Register in the table and on the parent's children list.
        // Both happen on the runtime so we can hold an async lock; we
        // do this synchronously by spawning a task — but that creates a
        // race with the just-launched body. Instead, hold a parking
        // operation: we use blocking_lock in a tokio::task::block_in_place
        // — but block_in_place requires a multi_thread runtime. Cleanest:
        // do the registration after spawn_body using a tokio::spawn that
        // runs immediately, and gate snapshot publish on it.
        //
        // Practical approach: do the locks via try_lock; they should be
        // uncontended at this point because no other code holds them.
        // Fall back to spawning a task if try_lock fails (shouldn't in
        // practice).
        let to_register = exec_arc.clone();
        let parent_for_link = if parent_id != TaskId::ROOT
            || self.root.id == TaskId::ROOT
        {
            // Look up the parent. Handled below.
            Some(parent_id)
        } else {
            None
        };
        let engine_clone = self.clone();
        tokio::spawn(async move {
            // Insert into table.
            engine_clone
                .table
                .lock()
                .await
                .insert(to_register.id, to_register.clone());
            // Push onto parent's children list.
            if let Some(pid) = parent_for_link
                && let Some(parent) = engine_clone.lookup(pid).await
            {
                parent.children.lock().await.push(to_register.clone());
            }
            // Snapshot publish.
            engine_clone.publish_snapshot().await;
        });

        // Spawn the watchdog, if any (arch.md §11).
        if let Some(d) = opts.timeout {
            let engine_w = self.clone();
            let id_w = id;
            let watchdog = tokio::spawn(async move {
                tokio::time::sleep(d).await;
                engine_w.timeout_task(id_w).await;
            });
            let abort = watchdog.abort_handle();
            let exec_for_abort = exec_arc.clone();
            tokio::spawn(async move {
                *exec_for_abort.watchdog_abort.lock().await = Some(abort);
            });
        }

        Ok(TaskHandle::new(exec_arc, Arc::downgrade(self)))
    }

    /// Cancel one task — single-task ladder (arch.md §7).
    ///
    /// Used by `TaskHandle::Drop` and as the per-node body of
    /// `cancel_subtree`. Does NOT walk children.
    pub async fn cancel_task(self: &Arc<Self>, id: TaskId) {
        self.cancel_task_with(id, CANCEL_TIMEOUT).await;
    }

    pub async fn cancel_task_with(
        self: &Arc<Self>,
        id: TaskId,
        kill_timeout: Duration,
    ) {
        let Some(exec) = self.lookup(id).await else {
            return;
        };

        // Abort the watchdog first so a Cancel→Timeout race doesn't
        // overwrite Cancelled with Timeout (arch.md §11).
        if let Some(h) = exec.watchdog_abort.lock().await.take() {
            h.abort();
        }

        // 1. Signal the token (cooperative, no-op if no opt-in observer).
        exec.cancellation.cancel();

        // 2. Stop the task's owned process groups via ctx.stop_all
        //    (arch.md §7).
        if let Some(ctx) = exec.task_context().await {
            ctx.stop_all(kill_timeout).await;
        }

        // 3. Wait `CANCEL_TIMEOUT` for the body's tokio task to exit.
        let join = exec.task_handle.lock().await.take();
        let abort_handle = exec.abort_handle.clone();
        if let Some(handle) = join {
            if tokio::time::timeout(CANCEL_TIMEOUT, handle).await.is_err() {
                // 4. Still alive — abort the tokio task.
                if let Some(ah) = abort_handle {
                    ah.abort();
                }
            }
        }

        // 5. Mark Cancelled (only if not already terminal).
        {
            let mut s = exec.task_status().lock().await;
            if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
                *s = TaskStatus::Cancelled;
            }
        }

        self.publish_snapshot().await;
    }

    /// Cancel a subtree (arch.md §7). Walks `children` and runs the
    /// single-task ladder on each.
    pub async fn cancel_subtree(self: &Arc<Self>, root: TaskId) {
        self.cancel_subtree_with(root, CANCEL_TIMEOUT).await;
    }

    pub async fn cancel_subtree_with(
        self: &Arc<Self>,
        root: TaskId,
        kill_timeout: Duration,
    ) {
        // BFS via an explicit stack.
        let mut stack = vec![root];
        let mut visited: Vec<TaskId> = Vec::new();
        while let Some(id) = stack.pop() {
            let kids: Vec<TaskId> = match self.lookup(id).await {
                Some(exec) => exec.children.lock().await.iter().map(|c| c.id).collect(),
                None => continue,
            };
            visited.push(id);
            stack.extend(kids);
        }
        // Cancel in reverse so leaves get hit first; matches "subtree
        // teardown" semantics and avoids parent-aborts-before-child
        // oddness.
        for id in visited.into_iter().rev() {
            self.cancel_task_with(id, kill_timeout).await;
        }
    }

    /// Cancel each direct child of root. Root stays alive (arch.md §7).
    pub async fn kill_all(self: &Arc<Self>) {
        let direct: Vec<TaskId> = match self.lookup(TaskId::ROOT).await {
            Some(root) => root.children.lock().await.iter().map(|c| c.id).collect(),
            None => return,
        };
        for id in direct {
            self.cancel_subtree(id).await;
        }
    }

    /// Sibling of `cancel_task` that writes `TaskStatus::Timeout`
    /// instead of `Cancelled` (arch.md §11). Called by per-task
    /// watchdog tokio tasks.
    pub async fn timeout_task(self: &Arc<Self>, id: TaskId) {
        let Some(exec) = self.lookup(id).await else {
            return;
        };
        // Don't re-abort the watchdog here — it is the caller.
        exec.cancellation.cancel();
        if let Some(ctx) = exec.task_context().await {
            ctx.stop_all(CANCEL_TIMEOUT).await;
        }
        let join = exec.task_handle.lock().await.take();
        let abort_handle = exec.abort_handle.clone();
        if let Some(handle) = join {
            if tokio::time::timeout(CANCEL_TIMEOUT, handle).await.is_err() {
                if let Some(ah) = abort_handle {
                    ah.abort();
                }
            }
        }
        {
            let mut s = exec.task_status().lock().await;
            if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
                *s = TaskStatus::Timeout;
            }
        }
        self.publish_snapshot().await;
    }
}

/// The engine. Owns the synthetic root, the table, the log store, and
/// the control channel. Created by [`Engine::start`].
pub struct Engine {
    internals: Arc<EngineInternals>,
    root_join: JoinHandle<TaskResult>,
}

/// Public handle returned to frontends. Cheap to clone.
#[derive(Clone)]
pub struct EngineHandle {
    pub(crate) internals: Arc<EngineInternals>,
    pub graph: watch::Receiver<GraphSnapshot>,
    pub log_store: Arc<Mutex<LogStore>>,
    pub registry: Arc<Registry>,
    pub root: TaskId,
}

impl EngineHandle {
    /// Configure a spawn. Returns a builder; `.timeout(d)`, `.spawn()`,
    /// or `.await` to fire it.
    pub fn spawn_task(
        &self,
        def: &'static TaskDef,
        args: Vec<String>,
    ) -> EngineSpawnBuilder {
        EngineSpawnBuilder {
            handle: self.clone(),
            def,
            args,
            timeout: None,
        }
    }

    /// Cancel one task and its subtree (engine-walked, arch.md §7).
    pub async fn kill_task(
        &self,
        id: TaskId,
        signal: KillSignal,
    ) -> Result<(), EngineError> {
        let (tx, rx) = oneshot::channel();
        self.internals
            .control_tx
            .send(Control::KillTask {
                id,
                signal,
                reply: tx,
            })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }

    /// Cancel each direct child of root. Root itself stays alive.
    pub async fn kill_all(&self) -> Result<(), EngineError> {
        let (tx, rx) = oneshot::channel();
        self.internals
            .control_tx
            .send(Control::KillAll { reply: tx })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }

    /// Shut down the runtime: cancel root subtree, then root body returns.
    pub async fn quit(&self) -> Result<(), EngineError> {
        let (tx, rx) = oneshot::channel();
        self.internals
            .control_tx
            .send(Control::Quit { reply: tx })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }

    /// Subscribe to log entries from any task or process.
    pub async fn subscribe_logs(&self) -> broadcast::Receiver<LogEntry> {
        self.log_store.lock().await.subscribe()
    }

    /// Look up a task in the graph.
    pub async fn lookup(&self, id: TaskId) -> Option<Arc<TaskExecution>> {
        self.internals.lookup(id).await
    }
}

/// Builder returned by `EngineHandle::spawn_task`.
pub struct EngineSpawnBuilder {
    handle: EngineHandle,
    def: &'static TaskDef,
    args: Vec<String>,
    timeout: Option<Duration>,
}

impl EngineSpawnBuilder {
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Spawn the task. Resolves to `Ok(TaskId)` once registered.
    pub async fn spawn(self) -> Result<TaskId, EngineError> {
        let (tx, rx) = oneshot::channel();
        let opts = SpawnOptions {
            timeout: self.timeout,
        };
        self.handle
            .internals
            .control_tx
            .send(Control::SpawnTask {
                def: self.def,
                args: self.args,
                opts,
                reply: tx,
            })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }
}

impl IntoFuture for EngineSpawnBuilder {
    type Output = Result<TaskId, EngineError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.spawn())
    }
}

impl Engine {
    /// Start the engine. Spawns the synthetic root task and returns the
    /// handle frontends use to send control and read state.
    pub fn start(registry: Arc<Registry>) -> (Self, EngineHandle) {
        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
        let (graph_tx, graph_rx) = watch::channel(GraphSnapshot::default());
        let log_store = Arc::new(Mutex::new(LogStore::new()));
        let tracing_installed = Arc::new(AtomicBool::new(false));

        // Build the root TaskExecution. Note: at this point we don't
        // have an Arc<EngineInternals> yet — we'll thread the weak in
        // after the Arc is constructed below via a self-cyclic
        // construction.
        //
        // Trick: build internals with a placeholder root, then replace
        // the root's wiring by re-launching once internals exists.
        // Simpler: use Arc::new_cyclic on EngineInternals so the root
        // can take a Weak<EngineInternals> at construction time.
        let internals = Arc::new_cyclic(|engine_weak: &std::sync::Weak<EngineInternals>| {
            // Build the root TaskExecution with an engine weak so the
            // monitor_spawns loop can publish snapshots. The root has
            // TaskId::ROOT.
            let root_exec_inner = TaskExecution::with_log_store_and_engine(
                TaskId::ROOT,
                log_store.clone(),
                engine_weak.clone(),
            );
            let mut root_exec_inner = root_exec_inner;
            root_exec_inner.set_registry(registry.clone());
            root_exec_inner.set_tracing_installed(tracing_installed.clone());

            let root_arc = Arc::new_cyclic(|self_weak: &std::sync::Weak<TaskExecution>| {
                let mut e = root_exec_inner;
                // Launch the root body via spawn_body so the root has
                // a real TaskContext, JoinHandle, and tracing wiring —
                // exactly like any child task per arch.md §6.
                e.spawn_body(self_weak.clone(), engine_weak.clone(), &ROOT_TASK, vec![]);
                e
            });

            EngineInternals {
                root: root_arc,
                table: Mutex::new(HashMap::new()),
                graph_tx,
                log_store: log_store.clone(),
                control_tx: control_tx.clone(),
                control_rx_slot: Mutex::new(Some(control_rx)),
                registry: registry.clone(),
                tracing_installed,
            }
        });

        // Insert root into the table.
        {
            let internals_clone = internals.clone();
            let root_clone = internals.root.clone();
            tokio::spawn(async move {
                internals_clone
                    .table
                    .lock()
                    .await
                    .insert(TaskId::ROOT, root_clone);
                internals_clone.publish_snapshot().await;
            });
        }

        // Take the root's JoinHandle so Engine::shutdown can await it.
        let root_join = {
            let mut slot = internals
                .root
                .task_handle
                .try_lock()
                .expect("root task_handle uncontended at start");
            slot.take()
                .expect("root spawn_body must have populated task_handle")
        };

        let handle = EngineHandle {
            internals: internals.clone(),
            graph: graph_rx,
            log_store: internals.log_store.clone(),
            registry: internals.registry.clone(),
            root: TaskId::ROOT,
        };

        let engine = Engine {
            internals,
            root_join,
        };

        (engine, handle)
    }

    /// Cancel the root externally (used in testing or last-resort
    /// shutdown). The canonical shutdown is `EngineHandle::quit().await`.
    #[allow(dead_code)]
    pub fn cancel(&self) {
        self.internals.root.cancellation.cancel();
    }

    /// Wait for the root task to finish. Returns when the runtime is
    /// fully shut down.
    pub async fn shutdown(self) {
        let _ = self.root_join.await;
    }
}

/// Take the control receiver out of `EngineInternals::control_rx_slot`.
/// Called by the synthetic root's body on first run.
pub(crate) async fn take_control_rx(
    engine: &Arc<EngineInternals>,
) -> Option<mpsc::UnboundedReceiver<Control>> {
    engine.control_rx_slot.lock().await.take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Registry, TaskContext, TaskFnKind};
    use std::pin::Pin;

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
        _ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        // Sleep without observing cancellation — the engine's ladder
        // step 4 (`abort_handle.abort()`) is the path that takes this
        // body down. Status writes happen on the engine side, mirroring
        // the realistic "task is hung, must be aborted" scenario.
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
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

    fn build_registry(extras: &[&'static TaskDef]) -> Arc<Registry> {
        let mut r = Registry::new();
        for d in extras {
            r.register(d);
        }
        Arc::new(r)
    }

    /// Wait for `id` to reach a terminal status, polling the graph
    /// snapshot. Returns the final status, or panics on timeout.
    async fn wait_terminal(handle: &EngineHandle, id: TaskId) -> TaskStatus {
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
    async fn engine_start_creates_root_node() {
        let registry = build_registry(&[]);
        let (engine, handle) = Engine::start(registry);

        // Give the registration tokio::spawn a moment to insert root.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let snap = handle.graph.borrow().clone();
        assert!(
            snap.tasks.contains_key(&TaskId::ROOT),
            "snapshot must contain the root task"
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_task_runs_to_completion() {
        let registry = build_registry(&[&OK]);
        let (engine, handle) = Engine::start(registry);

        let id = handle
            .spawn_task(&OK, vec![])
            .await
            .expect("spawn_task should succeed");
        let status = wait_terminal(&handle, id).await;
        assert!(matches!(status, TaskStatus::Done));

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn kill_task_cancels_running_task() {
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);

        let id = handle
            .spawn_task(&SLOW, vec![])
            .await
            .expect("spawn_task should succeed");

        // Let it actually start.
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle
            .kill_task(id, KillSignal::Term)
            .await
            .expect("kill_task should succeed");

        let status = wait_terminal(&handle, id).await;
        assert!(
            matches!(status, TaskStatus::Cancelled),
            "expected Cancelled, got {status:?}"
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn timeout_task_writes_timeout_status() {
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);

        let id = handle
            .spawn_task(&SLOW, vec![])
            .timeout(Duration::from_millis(100))
            .await
            .expect("spawn_task should succeed");

        let status = wait_terminal(&handle, id).await;
        assert!(
            matches!(status, TaskStatus::Timeout),
            "expected Timeout, got {status:?}"
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_aborts_watchdog_no_timeout_overwrite() {
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);

        let id = handle
            .spawn_task(&SLOW, vec![])
            .timeout(Duration::from_secs(2))
            .await
            .expect("spawn_task should succeed");

        // Let it start, then kill it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle
            .kill_task(id, KillSignal::Term)
            .await
            .expect("kill_task should succeed");

        let status = wait_terminal(&handle, id).await;
        assert!(
            matches!(status, TaskStatus::Cancelled),
            "expected Cancelled (watchdog must be aborted), got {status:?}"
        );

        // Wait past the timeout deadline to make sure the watchdog
        // doesn't fire and overwrite the status.
        tokio::time::sleep(Duration::from_millis(2200)).await;

        let snap = handle.graph.borrow().clone();
        let node = snap.tasks.get(&id).expect("task must still be in graph");
        assert!(
            matches!(node.status, TaskStatus::Cancelled),
            "status must remain Cancelled after watchdog deadline; got {:?}",
            node.status
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn kill_all_leaves_root_alive() {
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);

        let _id = handle
            .spawn_task(&SLOW, vec![])
            .await
            .expect("spawn_task should succeed");
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.kill_all().await.expect("kill_all should succeed");

        // Root must still be alive (not Cancelled).
        tokio::time::sleep(Duration::from_millis(100)).await;
        let snap = handle.graph.borrow().clone();
        let root_node = snap.tasks.get(&TaskId::ROOT).expect("root in graph");
        assert!(
            !matches!(root_node.status, TaskStatus::Cancelled),
            "root must stay alive after kill_all; got {:?}",
            root_node.status
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn quit_walks_subtree_and_returns_root() {
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);

        let id = handle
            .spawn_task(&SLOW, vec![])
            .await
            .expect("spawn_task should succeed");
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.quit().await.expect("quit should succeed");
        engine.shutdown().await; // root body must return

        // After shutdown, the spawned child should have been cancelled.
        let snap = handle.graph.borrow().clone();
        let child = snap.tasks.get(&id).expect("child in graph");
        assert!(
            matches!(child.status, TaskStatus::Cancelled),
            "child should be cancelled after quit; got {:?}",
            child.status
        );
    }
}
