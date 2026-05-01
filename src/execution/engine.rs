//! The engine: synthetic root, task table, control loop, cancel ladder.
//!
//! Slice 4 lands the public surface — `Engine`, `EngineHandle`,
//! `EngineSpawnBuilder`, `GraphSnapshot`, and the cancel ladder
//! (`cancel_task`, `cancel_subtree`, `kill_all`, `timeout_task`).
//!
//! See `docs/runtime_engine_design.md` for the canonical engine reference.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nix::sys::signal;
use nix::unistd::Pid;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::error::{TaskError, TaskResult};
use crate::log::LogEntry;
use crate::log::store::LogStore;
use crate::task::{Registry, TaskDef};

use super::TaskId;
use super::control::{Control, EngineError, KillSignal, RestartError, SpawnOptions};
use super::execution::{ProcessInfo, ProcessStatus, TaskExecution, TaskStatus};
use super::handle::TaskHandle;
use super::root::ROOT_TASK;

/// Cancel ladder timeout — used as the per-step grace period in
/// `cancel_task_with` and as the default `kill_timeout`.
pub(crate) const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// Install the global `tracing` subscriber. Exactly one install site in
/// the codebase. The subscriber is a single `LogEntryLayer` that reads
/// `TASK_TRACING_CTX` (set by `spawn_body` for each task) to route events
/// to the right buffer with the right `TaskId`. Idempotent across
/// multiple `Engine::start` calls (a `Once` gates the install).
fn install_global_tracing_subscriber() {
    use std::sync::Once;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::Registry;

    use crate::tracing_layer::LogEntryLayer;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = Registry::default().with(LogEntryLayer::new());
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _ = tracing::dispatcher::set_global_default(dispatch);
    });
}

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

impl GraphSnapshot {
    /// Walk parent links to find the direct child of `ROOT` that owns `id`.
    /// Returns `Some(id)` if `id` is itself top-level. Returns `None` for
    /// `ROOT`, unknown ids, or nodes whose parent chain doesn't reach `ROOT`.
    pub fn top_level(&self, id: TaskId) -> Option<TaskId> {
        if id == TaskId::ROOT {
            return None;
        }
        let mut current = id;
        loop {
            let node = self.tasks.get(&current)?;
            match node.parent {
                Some(p) if p == TaskId::ROOT => return Some(current),
                Some(p) => current = p,
                None => return None,
            }
        }
    }

    /// Parent chain for `id`, immediate-parent first, excluding `ROOT`.
    /// Empty for `ROOT` and unknown ids.
    pub fn ancestors(&self, id: TaskId) -> Vec<TaskId> {
        let mut out = Vec::new();
        if id == TaskId::ROOT {
            return out;
        }
        let Some(mut node) = self.tasks.get(&id) else {
            return out;
        };
        while let Some(p) = node.parent {
            if p == TaskId::ROOT {
                break;
            }
            out.push(p);
            let Some(next) = self.tasks.get(&p) else {
                break;
            };
            node = next;
        }
        out
    }

    /// True if `id` is a direct child of `ROOT`.
    pub fn is_top_level(&self, id: TaskId) -> bool {
        if id == TaskId::ROOT {
            return false;
        }
        self.tasks
            .get(&id)
            .and_then(|n| n.parent)
            .map(|p| p == TaskId::ROOT)
            .unwrap_or(false)
    }

    /// Returns a map from every TaskId in the graph (tasks + processes)
    /// to its display label. For tasks, the label is the task name.
    /// For processes, the label is the command_label (falling back to
    /// task_name if empty).
    pub fn source_labels(&self) -> HashMap<TaskId, String> {
        let mut m = HashMap::new();
        for (id, node) in self.tasks.iter() {
            m.insert(*id, node.name.clone());
            for proc in &node.processes {
                let label = if proc.command_label.is_empty() {
                    proc.task_name.clone()
                } else {
                    proc.command_label.clone()
                };
                m.insert(proc.id, label);
            }
        }
        m
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
    /// Unique id in the unified TaskId space (arch.md decision 22).
    pub id: TaskId,
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
            id: info.id,
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
    /// Task table, keyed by `TaskId`. Uses a `std::sync::Mutex` so
    /// `spawn_child` can register synchronously before the body runs —
    /// otherwise `lookup(id)` would race with the just-spawned tokio task.
    /// Critical sections are tiny (insert/remove/clone-out an Arc).
    pub table: StdMutex<HashMap<TaskId, Arc<TaskExecution>>>,
    pub graph_tx: watch::Sender<GraphSnapshot>,
    pub log_store: Arc<Mutex<LogStore>>,
    pub(crate) control_tx: mpsc::UnboundedSender<Control>,
    /// Receiver slot for the synthetic root's control loop. Set during
    /// `Engine::start`; the root body takes it on first run.
    pub(crate) control_rx_slot: Mutex<Option<mpsc::UnboundedReceiver<Control>>>,
    pub registry: Arc<Registry>,
}

impl EngineInternals {
    /// Look up a task by id. Includes the synthetic root at `TaskId::ROOT`.
    pub fn lookup(&self, id: TaskId) -> Option<Arc<TaskExecution>> {
        self.table
            .lock()
            .expect("engine table poisoned")
            .get(&id)
            .cloned()
    }

    /// Walk `table` and broadcast a fresh `GraphSnapshot`. Best-effort —
    /// briefly holds the (sync) table mutex while cloning Arcs, then uses
    /// async locks for status/children/processes.
    pub async fn publish_snapshot(&self) {
        let snapshot_entries: Vec<(TaskId, Arc<TaskExecution>)> = {
            let table = self.table.lock().expect("engine table poisoned");
            table.iter().map(|(k, v)| (*k, v.clone())).collect()
        };
        let mut tasks = HashMap::with_capacity(snapshot_entries.len());
        for (id, exec) in snapshot_entries {
            let status = exec.task_status().lock().await.clone();
            let kids: Vec<TaskId> = exec.children.lock().await.iter().map(|c| c.id).collect();
            let procs: Vec<ProcessNodeInfo> = exec
                .processes()
                .lock()
                .await
                .iter()
                .map(ProcessNodeInfo::from_process)
                .collect();
            tasks.insert(
                id,
                TaskNode {
                    id,
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
    ///
    /// Registration (table insert + parent's `children` push) happens
    /// **synchronously, before** `spawn_body` launches. This eliminates
    /// the race where `lookup(id)` could return `None` for a task whose
    /// body had already started running — which used to be observable
    /// from `ctx.run` chains and from the cancel ladder.
    pub fn spawn_child(
        self: &Arc<Self>,
        parent_id: TaskId,
        def: &'static TaskDef,
        args: Vec<String>,
        opts: SpawnOptions,
    ) -> Result<TaskHandle, TaskError> {
        let id = TaskId::next();
        let log_store = self.log_store.clone();
        let engine_weak = Arc::downgrade(self);

        // Build the execution and wrap in Arc::new_cyclic so the body's
        // TaskContext holds a Weak<TaskExecution> back to its own node
        // (for `ctx.run` → parent.children attachment).
        let exec_arc = {
            let mut e =
                TaskExecution::with_log_store_and_engine(id, log_store, engine_weak.clone());
            e.parent = Some(parent_id);
            e.set_registry(self.registry.clone());
            Arc::new_cyclic(|self_weak| {
                let mut e = e;
                // Synchronous registration BEFORE spawn_body, so that any
                // immediate observation (lookup, snapshot, cancel ladder)
                // sees a consistent table.
                e.spawn_body(self_weak.clone(), engine_weak.clone(), def, args);
                e
            })
        };

        // Insert into the table and onto the parent's children list,
        // synchronously. The table mutex is sync (`std::sync::Mutex`);
        // children is async but uncontested at this point — `try_lock`
        // would always succeed but we defer to a brief blocking-on-async
        // trick: since `spawn_body` already pushed a tokio task and may
        // have triggered no other accesses, we hold the std mutex here
        // and push to children via a same-task `try_lock` retry loop.
        {
            let mut table = self.table.lock().expect("engine table poisoned");
            table.insert(id, exec_arc.clone());
        }
        // For parent.children: try_lock first; if contended (rare —
        // only the cancel-ladder walker would hold it), fall back to a
        // brief detached task. Either way, the new node is already in
        // `table`, so `lookup(id)` works immediately.
        if let Some(parent) = self.lookup(parent_id) {
            match parent.children.try_lock() {
                Ok(mut kids) => kids.push(exec_arc.clone()),
                Err(_) => {
                    let parent_clone = parent.clone();
                    let to_register = exec_arc.clone();
                    tokio::spawn(async move {
                        parent_clone.children.lock().await.push(to_register);
                    });
                }
            }
        }
        // Publish snapshot in the background — best-effort; uses async
        // locks for the per-node fields.
        let engine_clone = self.clone();
        tokio::spawn(async move {
            engine_clone.publish_snapshot().await;
        });

        // Spawn the watchdog, if any (arch.md §11). The abort handle is
        // stored synchronously into the std::sync::Mutex so a cancel
        // arriving immediately after `spawn_child` returns can find and
        // abort it without racing with a deferred write.
        if let Some(d) = opts.timeout {
            let engine_w = self.clone();
            let id_w = id;
            let watchdog = tokio::spawn(async move {
                tokio::time::sleep(d).await;
                engine_w.timeout_task(id_w).await;
            });
            let abort = watchdog.abort_handle();
            *exec_arc
                .watchdog_abort
                .lock()
                .expect("watchdog_abort poisoned") = Some(abort);
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

    pub async fn cancel_task_with(self: &Arc<Self>, id: TaskId, kill_timeout: Duration) {
        let Some(exec) = self.lookup(id) else {
            return;
        };

        // Abort the watchdog first so a Cancel→Timeout race doesn't
        // overwrite Cancelled with Timeout (arch.md §11).
        if let Some(h) = exec
            .watchdog_abort
            .lock()
            .expect("watchdog_abort poisoned")
            .take()
        {
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
        if let Some(handle) = join
            && tokio::time::timeout(CANCEL_TIMEOUT, handle).await.is_err()
        {
            // 4. Still alive — abort the tokio task.
            if let Some(ah) = abort_handle {
                ah.abort();
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

    pub async fn cancel_subtree_with(self: &Arc<Self>, root: TaskId, kill_timeout: Duration) {
        // Walk the subtree parent-first via BFS, recording each node in
        // visit order. Cancel in that same parent-first order so
        // subscribers observe the parent transition to `Cancelled`
        // before any of its children — matching arch.md §7 semantics.
        let mut queue: std::collections::VecDeque<TaskId> = std::collections::VecDeque::new();
        queue.push_back(root);
        let mut visited: Vec<TaskId> = Vec::new();
        while let Some(id) = queue.pop_front() {
            let kids: Vec<TaskId> = match self.lookup(id) {
                Some(exec) => exec.children.lock().await.iter().map(|c| c.id).collect(),
                None => continue,
            };
            visited.push(id);
            for k in kids {
                queue.push_back(k);
            }
        }
        for id in visited {
            self.cancel_task_with(id, kill_timeout).await;
        }
    }

    /// Cancel each direct child of root. Root stays alive (arch.md §7).
    pub async fn kill_all(self: &Arc<Self>) {
        let direct: Vec<TaskId> = match self.lookup(TaskId::ROOT) {
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
        let Some(exec) = self.lookup(id) else {
            return;
        };
        // Don't re-abort the watchdog here — it is the caller.
        exec.cancellation.cancel();
        if let Some(ctx) = exec.task_context().await {
            ctx.stop_all(CANCEL_TIMEOUT).await;
        }
        let join = exec.task_handle.lock().await.take();
        let abort_handle = exec.abort_handle.clone();
        if let Some(handle) = join
            && tokio::time::timeout(CANCEL_TIMEOUT, handle).await.is_err()
            && let Some(ah) = abort_handle
        {
            ah.abort();
        }
        {
            let mut s = exec.task_status().lock().await;
            if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
                *s = TaskStatus::Timeout;
            }
        }
        self.publish_snapshot().await;
    }

    /// Send a signal to a single spawned process, identified by its
    /// `TaskId` in the unified id space. Walks `table` to find the
    /// owning `TaskExecution`.
    ///
    /// - `KillSignal::Term`: SIGTERM, then poll for exit every 50ms up
    ///   to `CANCEL_TIMEOUT`, fall back to SIGKILL on any survivor.
    /// - `KillSignal::Kill`: SIGKILL immediately.
    ///
    /// Does **not** touch task status. The 250ms `monitor_spawns`
    /// signal-0 watcher detects the exit and updates the process's
    /// status + publishes a snapshot.
    ///
    /// No-op if the process can't be found (already exited, wrong id,
    /// or not yet registered).
    pub async fn kill_process(self: &Arc<Self>, process_id: TaskId, signal: KillSignal) {
        // Find which TaskExecution owns this process and capture its
        // pgid/pid in one async lock pass to keep critical sections small.
        let target: Option<(Option<i32>, Option<u32>)> = {
            let entries: Vec<Arc<TaskExecution>> = {
                let table = self.table.lock().expect("engine table poisoned");
                table.values().cloned().collect()
            };
            let mut found = None;
            for exec in entries {
                let procs = exec.processes().lock().await;
                if let Some(proc) = procs.iter().find(|p| p.id == process_id) {
                    if proc.status != ProcessStatus::Running {
                        return;
                    }
                    found = Some((proc.pgid, proc.pid));
                    break;
                }
            }
            found
        };

        let Some((pgid, pid)) = target else {
            return;
        };

        // Helper: send the given signal to pgid (preferred) or pid (fallback).
        let send = |sig: signal::Signal| {
            if let Some(pgid) = pgid {
                let _ = signal::killpg(Pid::from_raw(pgid), Some(sig));
            } else if let Some(pid) = pid {
                let _ = signal::kill(Pid::from_raw(pid as i32), Some(sig));
            }
        };

        // Helper: probe whether the target is still alive (signal-0).
        let alive = || -> bool {
            if let Some(pgid) = pgid {
                signal::killpg(Pid::from_raw(pgid), None).is_ok()
            } else if let Some(pid) = pid {
                signal::kill(Pid::from_raw(pid as i32), None).is_ok()
            } else {
                false
            }
        };

        match signal {
            KillSignal::Kill => {
                send(signal::Signal::SIGKILL);
            }
            KillSignal::Term => {
                send(signal::Signal::SIGTERM);
                let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
                loop {
                    if !alive() {
                        return;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                if alive() {
                    send(signal::Signal::SIGKILL);
                }
            }
        }
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
    pub fn spawn_task(&self, def: &'static TaskDef, args: Vec<String>) -> EngineSpawnBuilder {
        EngineSpawnBuilder {
            handle: self.clone(),
            def,
            args,
            timeout: None,
        }
    }

    /// Cancel one task and its subtree (engine-walked, arch.md §7).
    pub async fn kill_task(&self, id: TaskId, signal: KillSignal) -> Result<(), EngineError> {
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

    /// Send a signal to a single spawned process by its `TaskId`.
    ///
    /// Calls `EngineInternals::kill_process` directly — process kills do
    /// not need to be serialized through the root body's control loop
    /// (no graph mutation, no task lifecycle change). The 250ms
    /// `monitor_spawns` watcher detects the resulting exit and updates
    /// the process status / publishes a snapshot.
    ///
    /// No-op if the process is not found or already exited.
    pub async fn kill_process(
        &self,
        process_id: TaskId,
        signal: KillSignal,
    ) -> Result<(), EngineError> {
        self.internals.kill_process(process_id, signal).await;
        Ok(())
    }

    /// Restart a top-level task. Cancels the existing task and subtree
    /// (the cancelled node stays in the graph snapshot), then spawns a
    /// fresh sibling using the same `TaskDef` and args. Returns the new
    /// `TaskId`.
    pub async fn restart(&self, id: TaskId) -> Result<TaskId, RestartError> {
        let (tx, rx) = oneshot::channel();
        self.internals
            .control_tx
            .send(Control::RestartTask { id, reply: tx })
            .map_err(|_| RestartError::ShuttingDown)?;
        rx.await.map_err(|_| RestartError::ShuttingDown)?
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
    ///
    /// Synchronous per arch.md §5: the only contention path is
    /// `LogStore::push` (which doesn't hold the lock across an await).
    /// `try_lock` should always succeed; if it doesn't, we fall back to
    /// a brief blocking wait via `blocking_lock` (sync mutex semantics
    /// over the tokio mutex, fine here because contention is bounded).
    pub fn subscribe_logs(&self) -> broadcast::Receiver<LogEntry> {
        match self.log_store.try_lock() {
            Ok(store) => store.subscribe(),
            Err(_) => self.log_store.blocking_lock().subscribe(),
        }
    }

    /// Look up a task in the graph (synchronous — table is a sync mutex).
    pub fn lookup(&self, id: TaskId) -> Option<Arc<TaskExecution>> {
        self.internals.lookup(id)
    }

    /// All source `TaskId`s belonging to `task_id` and any descendant
    /// (tasks AND processes). Used by frontends focusing a non-leaf task
    /// and wanting filtered logs (`LogStore::output_for_many`).
    pub fn source_ids_for(&self, task_id: TaskId) -> Vec<TaskId> {
        let snapshot = self.graph.borrow().clone();
        let mut out = Vec::new();
        let mut stack = vec![task_id];
        while let Some(id) = stack.pop() {
            let Some(node) = snapshot.tasks.get(&id) else {
                continue;
            };
            out.push(node.id);
            for proc in &node.processes {
                out.push(proc.id);
            }
            for &child in &node.children {
                stack.push(child);
            }
        }
        out
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
    ///
    /// Installs the global `tracing` subscriber here — exactly once per
    /// process. Multiple `Engine::start` calls (e.g. tests) all attempt
    /// to set the global default; subsequent calls are no-ops thanks to
    /// `set_global_default`'s built-in once semantics.
    pub fn start(registry: Arc<Registry>) -> (Self, EngineHandle) {
        // Install the global tracing subscriber. This is the only place
        // in the codebase that does it. Returns Err if a default was
        // already set (subsequent engines, test runs sharing a process);
        // we ignore that — the subscriber is idempotently good.
        install_global_tracing_subscriber();

        let (control_tx, control_rx) = mpsc::unbounded_channel::<Control>();
        let (graph_tx, graph_rx) = watch::channel(GraphSnapshot::default());
        let log_store = Arc::new(Mutex::new(LogStore::new()));

        // Build the engine internals via Arc::new_cyclic so the root
        // TaskExecution can hold a Weak<EngineInternals>.
        let internals = Arc::new_cyclic(|engine_weak: &std::sync::Weak<EngineInternals>| {
            let mut root_exec_inner = TaskExecution::with_log_store_and_engine(
                TaskId::ROOT,
                log_store.clone(),
                engine_weak.clone(),
            );
            root_exec_inner.set_registry(registry.clone());

            let root_arc = Arc::new_cyclic(|self_weak: &std::sync::Weak<TaskExecution>| {
                let mut e = root_exec_inner;
                e.spawn_body(self_weak.clone(), engine_weak.clone(), &ROOT_TASK, vec![]);
                e
            });

            EngineInternals {
                root: root_arc,
                table: StdMutex::new(HashMap::new()),
                graph_tx,
                log_store: log_store.clone(),
                control_tx: control_tx.clone(),
                control_rx_slot: Mutex::new(Some(control_rx)),
                registry: registry.clone(),
            }
        });

        // Insert root into the table synchronously.
        {
            let mut table = internals.table.lock().expect("engine table poisoned");
            table.insert(TaskId::ROOT, internals.root.clone());
        }
        {
            let internals_clone = internals.clone();
            tokio::spawn(async move {
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

    /// Task body that spawns a real long-running child process and then
    /// waits indefinitely. Used by `kill_process_*` tests to exercise
    /// per-process signalling without taking down the parent task.
    ///
    /// The handle is held but not awaited — `process::spawn`'s reaper
    /// task owns the underlying `tokio::process::Child` and reaps it as
    /// soon as it exits, so the engine's signal-0 exit watcher sees the
    /// death promptly even though this body never calls `wait()`.
    fn spawner_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            let _handle = ctx
                .spawn("sleep 60")
                .await
                .map_err(|e| crate::error::TaskError::from(format!("spawn failed: {e}")))?;
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

    fn parent_spawns_ok<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> {
        Box::pin(async move {
            // Spawn (not await) so the parent's body can return while
            // the child remains registered in the graph.
            let h = ctx.run("ok", &[]).spawn()?;
            // Detach so dropping the parent body doesn't cancel the child.
            tokio::spawn(async move {
                let _ = h.await;
            });
            Ok(())
        })
    }

    static PARENT_SPAWNS_OK: TaskDef = TaskDef {
        name: "parent_spawns_ok",
        description: None,
        group: "",
        func: TaskFnKind::Static(parent_spawns_ok),
        arg_metadata: no_args,
        ui_hint: None,
    };

    static SPAWNER: TaskDef = TaskDef {
        name: "spawner",
        description: None,
        group: "",
        func: TaskFnKind::Static(spawner_task),
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
    async fn kill_process_signals_pgid_and_leaves_task_running() {
        let registry = build_registry(&[&SPAWNER]);
        let (engine, handle) = Engine::start(registry);

        let task_id = handle
            .spawn_task(&SPAWNER, vec![])
            .await
            .expect("spawn_task should succeed");

        // Wait for the child process to register in the graph.
        let process_id = {
            let mut graph = handle.graph.clone();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let snap = graph.borrow().clone();
                if let Some(node) = snap.tasks.get(&task_id)
                    && let Some(proc) = node.processes.first()
                {
                    break proc.id;
                }
                tokio::select! {
                    _ = graph.changed() => {}
                    _ = tokio::time::sleep_until(deadline) => {
                        panic!("spawned process never registered in graph");
                    }
                }
            }
        };

        // SIGTERM the process. Engine walks the table, finds the owning
        // task, signals the pgid. monitor_spawns' 250ms watcher then
        // detects the exit and republishes the snapshot.
        handle
            .kill_process(process_id, KillSignal::Term)
            .await
            .expect("kill_process should succeed");

        // Wait for the process to flip to a non-Running status.
        let mut graph = handle.graph.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let snap = graph.borrow().clone();
            if let Some(node) = snap.tasks.get(&task_id)
                && let Some(proc) = node.processes.iter().find(|p| p.id == process_id)
                && proc.status != ProcessStatus::Running
            {
                break;
            }
            tokio::select! {
                _ = graph.changed() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("process never exited after kill_process");
                }
            }
        }

        // Parent task must still be alive (not Cancelled) — kill_process
        // only signals the process, not the task.
        let snap = handle.graph.borrow().clone();
        let task_node = snap.tasks.get(&task_id).expect("task in graph");
        assert!(
            !matches!(task_node.status, TaskStatus::Cancelled),
            "task must stay alive after kill_process; got {:?}",
            task_node.status
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn kill_process_unknown_id_is_noop() {
        let registry = build_registry(&[]);
        let (engine, handle) = Engine::start(registry);

        // Use an arbitrary id that won't match any process.
        handle
            .kill_process(TaskId(99_999), KillSignal::Term)
            .await
            .expect("kill_process on unknown id should still return Ok");

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    // Restart slice tests
    #[tokio::test]
    async fn restart_top_level_returns_new_id_and_keeps_old_subtree() {
        let registry = build_registry(&[&SLOW]);
        let (engine, handle) = Engine::start(registry);

        let old_id = handle
            .spawn_task(&SLOW, vec![])
            .await
            .expect("spawn_task should succeed");
        // Let it actually start.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let new_id = handle.restart(old_id).await.expect("restart should succeed");
        assert_ne!(old_id, new_id, "restart must return a fresh TaskId");

        // Old must reach Cancelled and remain in the graph.
        let status = wait_terminal(&handle, old_id).await;
        assert!(
            matches!(status, TaskStatus::Cancelled),
            "old task must be cancelled; got {status:?}"
        );
        let snap = handle.graph.borrow().clone();
        assert!(
            snap.tasks.contains_key(&old_id),
            "old TaskNode must remain in the snapshot"
        );
        assert!(
            snap.tasks.contains_key(&new_id),
            "new TaskNode must be present"
        );

        // New task is also a direct child of ROOT.
        let new_node = snap.tasks.get(&new_id).expect("new node");
        assert_eq!(new_node.parent, Some(TaskId::ROOT));

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn restart_non_top_level_errors() {
        let registry = build_registry(&[&OK, &PARENT_SPAWNS_OK]);
        let (engine, handle) = Engine::start(registry);

        let parent_id = handle
            .spawn_task(&PARENT_SPAWNS_OK, vec![])
            .await
            .expect("spawn parent");
        let _ = wait_terminal(&handle, parent_id).await;
        // Allow snapshot publish for the child.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let snap = handle.graph.borrow().clone();
        let parent_node = snap.tasks.get(&parent_id).expect("parent in graph");
        assert_eq!(parent_node.children.len(), 1);
        let child_id = parent_node.children[0];

        let err = handle
            .restart(child_id)
            .await
            .expect_err("restart on non-top-level must error");
        assert!(
            matches!(err, RestartError::NotTopLevel(id) if id == child_id),
            "expected NotTopLevel; got {err:?}"
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn restart_unknown_id_errors_not_found() {
        let registry = build_registry(&[]);
        let (engine, handle) = Engine::start(registry);

        let err = handle
            .restart(TaskId(99_999))
            .await
            .expect_err("restart on unknown id must error");
        assert!(
            matches!(err, RestartError::NotFound(_)),
            "expected NotFound; got {err:?}"
        );

        let _ = handle.quit().await;
        engine.shutdown().await;
    }

    #[test]
    fn graph_helpers_on_root() {
        let snap = GraphSnapshot::default();
        assert_eq!(snap.top_level(TaskId::ROOT), None);
        assert!(!snap.is_top_level(TaskId::ROOT));
        assert!(snap.ancestors(TaskId::ROOT).is_empty());
    }

    #[test]
    fn graph_helpers_on_unknown_id() {
        let snap = GraphSnapshot::default();
        let unknown = TaskId(12_345);
        assert_eq!(snap.top_level(unknown), None);
        assert!(!snap.is_top_level(unknown));
        assert!(snap.ancestors(unknown).is_empty());
    }

    #[test]
    fn graph_helpers_on_nested_ids() {
        // Build a synthetic snapshot: ROOT → top → mid → leaf
        let top = TaskId(1);
        let mid = TaskId(2);
        let leaf = TaskId(3);
        let mut tasks = HashMap::new();
        tasks.insert(
            top,
            TaskNode {
                id: top,
                name: "top".into(),
                parent: Some(TaskId::ROOT),
                children: vec![mid],
                status: TaskStatus::Setup,
                processes: vec![],
            },
        );
        tasks.insert(
            mid,
            TaskNode {
                id: mid,
                name: "mid".into(),
                parent: Some(top),
                children: vec![leaf],
                status: TaskStatus::Setup,
                processes: vec![],
            },
        );
        tasks.insert(
            leaf,
            TaskNode {
                id: leaf,
                name: "leaf".into(),
                parent: Some(mid),
                children: vec![],
                status: TaskStatus::Setup,
                processes: vec![],
            },
        );
        let snap = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks),
        };

        assert_eq!(snap.top_level(top), Some(top));
        assert_eq!(snap.top_level(mid), Some(top));
        assert_eq!(snap.top_level(leaf), Some(top));
        assert!(snap.is_top_level(top));
        assert!(!snap.is_top_level(mid));
        assert!(!snap.is_top_level(leaf));
        assert!(snap.ancestors(top).is_empty());
        assert_eq!(snap.ancestors(mid), vec![top]);
        assert_eq!(snap.ancestors(leaf), vec![mid, top]);
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
