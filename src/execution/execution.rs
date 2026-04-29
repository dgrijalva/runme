//! Shared execution layer for task lifecycle management.
//!
//! `TaskExecution` is the unit of task execution. Slice 2 reshapes it into
//! the recursive node described in `docs/plans/notes/architecture.md` §2:
//! it now carries an identity (`id`, `parent`, `children`), an independent
//! `CancellationToken`, and a `JoinHandle<TaskResult>` slot so the awaited
//! handle (slice 3) can return the body's result without a side channel.
//!
//! The `LogStore` is no longer constructed here — slice 2 makes it caller-
//! provided so the eventual `Engine` (slice 4) can own a single store
//! shared across the whole graph.

use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::signal;
use nix::unistd::Pid;
use tokio::sync::{Mutex, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;

use crate::error::TaskResult;
use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::log::store::LogStore;
use crate::task::{Registry, SpawnEvent, TaskContext, TaskDef};
use crate::tracing_layer::LogEntryLayer;

use super::task_id::TaskId;

// ============================================================
// Task and process lifecycle types
// ============================================================

/// Status of a running task.
///
/// `Cancelled` and `Timeout` are siblings of `Done`/`Failed` — the engine
/// writes them after the cancel ladder (slice 4) finishes. They land in
/// slice 2 alongside the rest of the status reshape so consumers (TUI
/// rendering, status transitions) only need to be updated once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task function is still executing (spawning processes, doing setup).
    Setup,
    /// Task function returned Ok, but spawned processes are still running.
    Ready,
    /// Task function returned Ok and no processes remain.
    Done,
    /// Task function returned an error or panicked.
    Failed(TaskFailure),
    /// Task was cancelled by an explicit user/engine action (e.g. KillTask,
    /// TaskHandle::Drop). Wired by the cancel ladder in slice 4.
    Cancelled,
    /// Per-task timeout watchdog fired. Wired by the timeout watchdog in
    /// slice 4.
    Timeout,
}

/// Details of a task failure.
///
/// Preserves the structured error output and exit code from `TaskError`
/// so that Agent mode can report them faithfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFailure {
    /// Human-readable error message (from `TaskError::to_string()`).
    pub message: String,
    /// Suggested exit code.
    pub exit_code: i32,
    /// Structured error output (JSON), serialized as a string to keep
    /// `TaskStatus` Clone + Eq without depending on serde_json::Value.
    pub output_json: String,
}

/// Status of an individual spawned process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Process is currently running.
    Running,
    /// Process exited successfully (exit code 0).
    Done,
    /// Process exited with a non-zero exit code, was killed by a signal, or timed out.
    Failed(crate::process::Termination),
    /// Process was stopped by the user.
    Stopped,
}

/// Information about a spawned process.
///
/// Tracks process identity (pid, pgid), lifecycle status, output buffer,
/// and display metadata. Owned by the execution layer; read by UI for display.
pub struct ProcessInfo {
    pub task_name: String,
    pub command_label: String,
    pub buffer: Arc<Mutex<OutputBuffer>>,
    pub pgid: Option<i32>,
    pub pid: Option<u32>,
    pub status: ProcessStatus,
    /// Whether the process's readiness probe has succeeded.
    pub ready: bool,
}

impl ProcessInfo {
    /// Check if this process is still running by sending signal 0 to the pid.
    /// Updates self.status if the process has exited and was previously Running.
    pub fn refresh_status(&mut self) {
        if self.status != ProcessStatus::Running {
            return;
        }
        if let Some(pid) = self.pid {
            match signal::kill(Pid::from_raw(pid as i32), None) {
                Ok(()) => {} // still running
                Err(_) => {
                    self.status = ProcessStatus::Done;
                }
            }
        }
    }

    /// Get a short display label for this process.
    pub fn display_name(&self) -> &str {
        &self.command_label
    }
}

// ============================================================
// Output forwarding helpers
// ============================================================

/// Forward entries from any OutputBuffer to the LogStore via broadcast subscription.
pub(crate) fn start_buffer_forwarder(
    buffer: &tokio::sync::Mutex<OutputBuffer>,
    log_store: Arc<Mutex<LogStore>>,
) {
    let rx = buffer.try_lock().map(|buf| buf.subscribe());
    if let Ok(mut rx) = rx {
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(entry) => {
                        log_store.lock().await.push(entry);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }
}

/// Forward entries from a tracing OutputBuffer to the LogStore.
///
/// The receiver must be created BEFORE launching the task to avoid
/// missing early entries (broadcast only delivers messages sent after subscribe).
pub(crate) fn start_tracing_forwarder(
    rx: tokio::sync::broadcast::Receiver<LogEntry>,
    log_store: Arc<Mutex<LogStore>>,
) {
    let mut rx = rx;
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    log_store.lock().await.push(entry);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
}

// ============================================================
// TaskExecution
// ============================================================

/// A single task execution. Owns the full lifecycle from launch to cleanup.
///
/// In the multi-task runtime this is a node in the engine's task graph: it
/// has an identity (`id`, `parent`), a list of `children`, and an
/// independent `CancellationToken`. Parent-child propagation is performed
/// by the engine walking the graph (slice 4) — tokens are NOT linked via
/// `child_token()`. See "Cancellation model" in `architecture.md`.
pub struct TaskExecution {
    // ── Identity ───────────────────────────────────────────────────
    /// Unique id for this execution, allocated from `TaskId::next`.
    pub id: TaskId,
    /// Name of the task being executed.
    pub task_name: String,
    /// Parent's id. `None` only for the synthetic root.
    pub parent: Option<TaskId>,

    // ── Graph ──────────────────────────────────────────────────────
    /// Direct children of this execution. Slice 3 populates this when
    /// `ctx.run` materialises a child node; slice 2 leaves it empty so
    /// the single-task path keeps working unchanged.
    pub children: Arc<Mutex<Vec<Arc<TaskExecution>>>>,

    // ── Lifecycle ──────────────────────────────────────────────────
    /// Cooperative cancellation signal. **Independent — NOT a child token
    /// of the parent's.** Constructed via `CancellationToken::new()`. The
    /// engine propagates cancellation by walking `children` explicitly
    /// (slice 4).
    pub cancellation: CancellationToken,
    /// Current task status, shared with the running `TaskContext` so
    /// `bind_ready` / `mark_ready` updates land here. The engine writes
    /// `Cancelled` / `Timeout` after running its respective ladders.
    task_status: Arc<Mutex<TaskStatus>>,
    /// `JoinHandle` for the spawned task body. Populated by `launch`,
    /// taken (consumed) when the handle's future is awaited (slice 3) or
    /// when the cancel ladder reclaims it (slice 4).
    pub task_handle: Mutex<Option<JoinHandle<TaskResult>>>,
    /// Cheap clone of the task body's `AbortHandle`. Used by the cancel
    /// ladder's step 4 (slice 4) without needing to lock the JoinHandle
    /// slot.
    pub abort_handle: Option<AbortHandle>,
    /// Abort handle for the per-task timeout watchdog tokio task. `None`
    /// when no timeout is configured. Aborted by the cancel ladder
    /// (slice 4) to prevent Cancel→Timeout races.
    pub watchdog_abort: Mutex<Option<AbortHandle>>,

    // ── Process tracking ───────────────────────────────────────────
    /// Spawned processes tracked by this execution.
    processes: Arc<Mutex<Vec<ProcessInfo>>>,
    /// Sender for spawn events (given to the TaskContext).
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,

    // ── Logging ────────────────────────────────────────────────────
    /// Aggregated log store for all output. Engine-owned in the multi-
    /// task runtime — callers always supply this.
    log_store: Arc<Mutex<LogStore>>,
    /// The task's tracing output buffer (info!/error!/etc from the task
    /// function).
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    /// Whether the global tracing subscriber has been installed. Hoisted
    /// to the engine in slice 4; held here as an `Arc<AtomicBool>` so
    /// callers can share a single flag across multiple executions today.
    tracing_installed: Arc<AtomicBool>,

    // ── Registry ───────────────────────────────────────────────────
    /// Optional shared registry for task discovery and cross-invocation.
    registry: Option<Arc<Registry>>,
}

impl TaskExecution {
    /// Create a new `TaskExecution` that shares an existing `LogStore`.
    ///
    /// `id` is the caller's responsibility — for fresh top-level tasks
    /// pass `TaskId::next()`. The execution starts with `parent: None`
    /// and an empty `children` list; slice 3 wires those up when child
    /// invocations materialise as graph nodes.
    pub fn with_log_store(id: TaskId, log_store: Arc<Mutex<LogStore>>) -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let processes = Arc::new(Mutex::new(Vec::new()));

        let monitor_log_store = log_store.clone();
        let monitor_processes = processes.clone();
        tokio::spawn(async move {
            Self::monitor_spawns(spawn_rx, monitor_log_store, monitor_processes).await;
        });

        Self {
            id,
            task_name: String::new(),
            parent: None,
            children: Arc::new(Mutex::new(Vec::new())),
            cancellation: CancellationToken::new(),
            task_status: Arc::new(Mutex::new(TaskStatus::Setup)),
            task_handle: Mutex::new(None),
            abort_handle: None,
            watchdog_abort: Mutex::new(None),
            processes,
            spawn_tx,
            log_store,
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            tracing_installed: Arc::new(AtomicBool::new(false)),
            registry: None,
        }
    }

    /// Set the task registry for cross-invocation and discovery.
    pub fn set_registry(&mut self, registry: Arc<Registry>) {
        self.registry = Some(registry);
    }

    /// Share a `tracing_installed` flag across multiple executions.
    ///
    /// `set_global_default` can only succeed once per process. When
    /// launching multiple executions (e.g. TUI picker), they should
    /// share this flag. Slice 4 hoists this to the engine.
    pub fn set_tracing_installed(&mut self, flag: Arc<AtomicBool>) {
        self.tracing_installed = flag;
    }

    /// Launch a task. Creates the `TaskContext`, installs tracing,
    /// subscribes to all output buffers, and spawns the task function.
    ///
    /// The `JoinHandle<TaskResult>` is stored in `task_handle`. Slice 3's
    /// `TaskHandle::IntoFuture` takes it to recover the body's result;
    /// slice 4's cancel ladder uses the cached `abort_handle` to abort
    /// without re-locking the slot.
    pub fn launch(&mut self, task: &'static TaskDef, task_args: Vec<String>) {
        self.launch_with_self_weak(Weak::new(), task, task_args);
    }

    /// Launch with a `Weak<TaskExecution>` reference to this very node.
    ///
    /// Slice 3 wires the weak ref into the `TaskContext` so that
    /// `ctx.run` can attach children to the parent's `children` list.
    /// Pass `Weak::new()` (or call `launch`) when no self-reference is
    /// needed — e.g. top-level test launches with no graph parent.
    pub fn launch_with_self_weak(
        &mut self,
        self_weak: Weak<TaskExecution>,
        task: &'static TaskDef,
        task_args: Vec<String>,
    ) {
        self.task_name = task.name.to_string();

        let tracing_buffer = self.tracing_buffer.clone();
        let log_store = self.log_store.clone();
        let spawn_tx = self.spawn_tx.clone();

        // Install the LogEntryLayer as the tracing subscriber for this task.
        let layer = LogEntryLayer::new(tracing_buffer.clone(), task.name);
        let subscriber = tracing_subscriber::registry().with(layer);

        // Create the TaskContext and wire everything up.
        let mut ctx = TaskContext::new(task.name);
        ctx.set_spawn_notifier(spawn_tx);
        ctx.set_task_status(self.task_status.clone());
        if let Some(ref registry) = self.registry {
            ctx.set_registry(registry.clone());
        }
        ctx.set_tracing_output(tracing_buffer.clone());

        // Slice 3: wire identity + cancellation + self-weak so `ctx.run`
        // can attach children to this node's `children` list and
        // expose `ctx.cancelled()` / `ctx.cancellation()` to the body.
        ctx.set_task_identity(self.id, self.cancellation.clone(), self_weak);
        ctx.set_log_store(self.log_store.clone());

        // Forward exec() output to LogStore.
        start_buffer_forwarder(ctx.output_buffer(), log_store.clone());

        // Subscribe to tracing buffer BEFORE installing the subscriber
        // and spawning the task, so we don't miss early entries.
        let tracing_rx = {
            let buf = tracing_buffer
                .try_lock()
                .expect("tracing buffer not locked");
            buf.subscribe()
        };
        start_tracing_forwarder(tracing_rx, log_store.clone());

        // Install the global default tracing subscriber only once.
        if !self.tracing_installed.swap(true, Ordering::SeqCst) {
            let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
            let _ = tracing::dispatcher::set_global_default(dispatch);
        }

        // Spawn the task function. The JoinHandle now carries the body's
        // `TaskResult` so the awaited TaskHandle (slice 3) can return it.
        // Status writes are still done here against the shared Arc so
        // observers see the in-flight transition.
        let task_status = self.task_status.clone();
        let handle: JoinHandle<TaskResult> = tokio::spawn(async move {
            let result = task.func.call(&ctx, &task_args).await;

            let mut s = task_status.lock().await;
            match &result {
                Ok(()) => {
                    *s = TaskStatus::Done;
                }
                Err(task_err) => {
                    let failure = TaskFailure {
                        message: task_err.to_string(),
                        exit_code: task_err.exit_code(),
                        output_json: task_err.output().to_string(),
                    };
                    tracing::error!("task failed: {}", failure.message);
                    *s = TaskStatus::Failed(failure);
                }
            }

            result
        });

        self.abort_handle = Some(handle.abort_handle());
        *self
            .task_handle
            .try_lock()
            .expect("task_handle slot uncontended at launch") = Some(handle);
    }

    /// Wait for the task function to complete.
    ///
    /// Does not stop spawned processes — call `shutdown()` for that.
    /// Returns the body's `TaskResult` if the slot was still populated;
    /// `None` if it was already taken (e.g. by an awaited `TaskHandle`).
    pub async fn wait(&self) -> Option<TaskResult> {
        let handle = {
            let mut slot = self.task_handle.lock().await;
            slot.take()
        };
        match handle {
            Some(h) => match h.await {
                Ok(res) => Some(res),
                Err(_) => None, // panic / abort — status was already set
            },
            None => None,
        }
    }

    /// Shut down all spawned processes.
    ///
    /// Sends SIGTERM to each process group, polls until they exit or the
    /// timeout expires, then sends SIGKILL to any survivors.
    pub async fn shutdown(&self, timeout: std::time::Duration) {
        let mut pgids: Vec<i32> = Vec::new();

        {
            let procs = self.processes.lock().await;
            for proc in procs.iter() {
                if proc.status == ProcessStatus::Running
                    && let Some(pgid) = proc.pgid
                    && !pgids.contains(&pgid)
                {
                    pgids.push(pgid);
                }
            }
        }

        if pgids.is_empty() {
            return;
        }

        // SIGTERM all process groups
        for &pgid in &pgids {
            let _ = signal::killpg(Pid::from_raw(pgid), Some(signal::Signal::SIGTERM));
        }

        // Poll until all are dead or timeout expires
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let all_dead = pgids
                .iter()
                .all(|&pgid| signal::killpg(Pid::from_raw(pgid), None).is_err());
            if all_dead {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // SIGKILL any survivors
        for &pgid in &pgids {
            let _ = signal::killpg(Pid::from_raw(pgid), Some(signal::Signal::SIGKILL));
        }
    }

    /// Check if any spawned processes are still running.
    pub async fn has_running_processes(&self) -> bool {
        let mut procs = self.processes.lock().await;
        for proc in procs.iter_mut() {
            proc.refresh_status();
        }
        procs.iter().any(|p| p.status == ProcessStatus::Running)
    }

    /// Access the LogStore for subscribing to output.
    pub fn log_store(&self) -> &Arc<Mutex<LogStore>> {
        &self.log_store
    }

    /// Access the task status.
    pub fn task_status(&self) -> &Arc<Mutex<TaskStatus>> {
        &self.task_status
    }

    /// Access the process list.
    pub fn processes(&self) -> &Arc<Mutex<Vec<ProcessInfo>>> {
        &self.processes
    }

    /// Subscribe to the LogStore's broadcast channel for new entries.
    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry> {
        self.log_store.lock().await.subscribe()
    }

    /// Background loop that receives spawn events, creates ProcessInfo,
    /// and sets up output forwarding to the LogStore.
    async fn monitor_spawns(
        mut spawn_rx: mpsc::UnboundedReceiver<SpawnEvent>,
        log_store: Arc<Mutex<LogStore>>,
        processes: Arc<Mutex<Vec<ProcessInfo>>>,
    ) {
        while let Some(event) = spawn_rx.recv().await {
            let buffer = event.buffer.clone();

            // Determine initial readiness
            let has_readiness_condition = event.readiness_rx.is_some();
            let ready = !has_readiness_condition; // Ready by default if no condition

            let process_info = ProcessInfo {
                task_name: event.task_name.clone(),
                command_label: event.command_label.clone(),
                buffer: event.buffer.clone(),
                pgid: event.pgid,
                pid: event.pid,
                status: ProcessStatus::Running,
                ready,
            };

            let process_index = {
                let mut procs = processes.lock().await;
                let idx = procs.len();
                procs.push(process_info);
                idx
            };

            // Watch for readiness changes if a condition was configured
            if let Some(mut readiness_rx) = event.readiness_rx {
                let processes = processes.clone();
                tokio::spawn(async move {
                    let _ = readiness_rx.wait_for(|&ready| ready).await;
                    if let Some(proc) = processes.lock().await.get_mut(process_index) {
                        proc.ready = true;
                    }
                });
            }

            // Subscribe to the process's output buffer and forward to LogStore.
            let store = log_store.clone();
            let mut rx = {
                let buf = buffer.lock().await;
                let mut s = store.lock().await;
                s.ingest_buffer(&buf);
                buf.subscribe()
            };

            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(entry) => {
                            store.lock().await.push(entry);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            });
        }
    }
}
