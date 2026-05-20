//! Shared execution layer for task lifecycle management.
//!
//! `TaskExecution` is the unit of task execution. Slice 2 reshapes it into
//! the recursive node described in `docs/runtime_engine_design.md` § Types — `TaskExecution`:
//! it now carries an identity (`id`, `parent`, `children`), an independent
//! `CancellationToken`, and a `JoinHandle<TaskResult>` slot so the awaited
//! handle (slice 3) can return the body's result without a side channel.
//!
//! The `LogStore` is no longer constructed here — slice 2 makes it caller-
//! provided so the eventual `Engine` (slice 4) can own a single store
//! shared across the whole graph.

use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};

use nix::sys::signal;
use nix::unistd::Pid;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::error::TaskResult;
use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::log::store::LogStore;
use crate::task::{Registry, SpawnEvent, TaskContext, TaskDef};
use crate::tracing_layer::{TaskTracingCtx, attach_task_tracing_ctx};

use super::engine::EngineInternals;
use super::invocation::Invocation;
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Unique id in the unified TaskId space (arch.md decision 22).
    /// Allocated when the process is spawned and used as the
    /// `LogEntry.source` for every entry produced by this process.
    pub id: TaskId,
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
    /// when no timeout is configured. Aborted by the cancel ladder to
    /// prevent Cancel→Timeout races. Stored under a `std::sync::Mutex`
    /// so `spawn_child` can populate it synchronously before returning
    /// the handle (eliminates the race where a cancel arriving
    /// immediately after spawn could miss the watchdog).
    pub watchdog_abort: StdMutex<Option<AbortHandle>>,

    // ── Process tracking ───────────────────────────────────────────
    /// Spawned processes tracked by this execution.
    processes: Arc<Mutex<Vec<ProcessInfo>>>,
    /// Sender for spawn events (given to the TaskContext).
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,

    // ── Runtime context (slice 4) ──────────────────────────────────
    /// `Arc<TaskContext>` shared with the running task body. Set inside
    /// `spawn_body`. Used by the cancel ladder to invoke `ctx.stop_all`
    /// per arch.md §7. `None` until `spawn_body` runs (i.e. for freshly
    /// constructed nodes that haven't launched yet).
    task_ctx: Mutex<Option<Arc<TaskContext>>>,

    // ── Logging ────────────────────────────────────────────────────
    /// Aggregated log store for all output. Engine-owned in the multi-
    /// task runtime — callers always supply this.
    log_store: Arc<Mutex<LogStore>>,
    /// The task's tracing output buffer (info!/error!/etc from the task
    /// function). The global `LogEntryLayer` (installed once in
    /// `Engine::start`) writes here when an event fires inside the
    /// per-task carrier span attached by `spawn_body`.
    tracing_buffer: Arc<Mutex<OutputBuffer>>,

    // ── Registry ───────────────────────────────────────────────────
    /// Optional shared registry for task discovery and cross-invocation.
    registry: Option<Arc<Registry>>,

    // ── Restart support ────────────────────────────────────────────
    /// The `TaskDef` and args this execution was launched with. Captured
    /// in `spawn_body` so `Engine::restart` can re-spawn the same task.
    /// `None` for the synthetic root.
    pub task_def: Option<&'static TaskDef>,
    pub task_args: Vec<String>,
    /// Sender side of the cooperative soft-restart signal. Shared with
    /// the running `TaskContext` (cloned in `spawn_body`). The engine
    /// fires signals through this; the task subscribes via
    /// `ctx.restart_handle()`. `receiver_count() > 0` is how the engine
    /// decides whether a soft restart is deliverable or should fall
    /// back to hard.
    pub restart_signal: Arc<watch::Sender<u64>>,

    // ── Reporting (timestamps + summary) ──────────────────────────
    /// Timestamp at which the task body began running. Set exactly once
    /// in `spawn_body` immediately before the body's tokio task is
    /// launched. `OnceLock::get()` returns `None` until set.
    pub started_at: Arc<OnceLock<chrono::DateTime<chrono::Local>>>,
    /// Timestamp at which the task reached a terminal status (Done /
    /// Failed / Cancelled / Timeout). Set exactly once at each terminal
    /// status writer.
    pub ended_at: Arc<OnceLock<chrono::DateTime<chrono::Local>>>,
    /// Optional human-readable summary written by the task body via
    /// `TaskContext::summary`. Last write wins. Shared with the running
    /// `TaskContext` (mirrors the `task_status` sharing pattern).
    pub summary: Arc<Mutex<Option<String>>>,
    /// If set by the cancel/timeout ladder before the body's tokio task
    /// observed cancellation, the body completion handler will use this
    /// status (Cancelled / Timeout) instead of the body's natural exit
    /// (Done / Failed). This makes user-requested kill/timeout
    /// "winning" over a cooperative `Ok(())` return — without this, a
    /// task that uses `ctx.cancellation_signal()` and returns Ok races
    /// the engine and reports `completed (exit 0)` despite the kill.
    pub terminal_override: Arc<OnceLock<TaskStatus>>,
}

impl TaskExecution {
    /// Create a new `TaskExecution` wired to an engine.
    ///
    /// `id` is the caller's responsibility — `EngineInternals::spawn_child`
    /// allocates one via `TaskId::next()`. The execution starts with
    /// `parent: None` (the engine sets it) and an empty `children` list.
    pub fn with_log_store_and_engine(
        id: TaskId,
        log_store: Arc<Mutex<LogStore>>,
        seq_gen: crate::log::SeqGen,
        engine: Weak<EngineInternals>,
    ) -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let processes = Arc::new(Mutex::new(Vec::new()));

        let monitor_log_store = log_store.clone();
        let monitor_processes = processes.clone();
        let monitor_engine = engine.clone();
        tokio::spawn(async move {
            Self::monitor_spawns(
                spawn_rx,
                monitor_log_store,
                monitor_processes,
                monitor_engine,
            )
            .await;
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
            watchdog_abort: StdMutex::new(None),
            processes,
            spawn_tx,
            task_ctx: Mutex::new(None),
            log_store,
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::with_seq_gen(10_000, seq_gen))),
            registry: None,
            task_def: None,
            task_args: Vec::new(),
            restart_signal: Arc::new(watch::Sender::new(0u64)),
            started_at: Arc::new(OnceLock::new()),
            ended_at: Arc::new(OnceLock::new()),
            summary: Arc::new(Mutex::new(None)),
            terminal_override: Arc::new(OnceLock::new()),
        }
    }

    /// Set the task registry for cross-invocation and discovery.
    pub fn set_registry(&mut self, registry: Arc<Registry>) {
        self.registry = Some(registry);
    }

    /// Engine-aware launch core. Builds the `TaskContext`, wires the
    /// engine weak into it, and runs the body instrumented by a per-task
    /// carrier span carrying the `TaskTracingCtx` in its extensions, so
    /// the global tracing layer routes events from inside the body to
    /// this task's buffer. Spans propagate across `tokio::spawn` when
    /// child futures are `.instrument(Span::current())`'d — the
    /// `rnme::spawn!` macro does this for the common case.
    pub fn spawn_body(
        &mut self,
        self_weak: Weak<TaskExecution>,
        engine: Weak<EngineInternals>,
        task: &'static TaskDef,
        invocation: Invocation,
    ) {
        self.task_name = task.name.to_string();
        self.task_def = Some(task);
        // `task_args` is populated only for `Invocation::Strings` — that's
        // the data the hard-restart path needs. Factory invocations
        // capture their typed args inside the closure (consumed below),
        // so `task_args` stays empty; a hard restart of a Factory task
        // would re-spawn through the string path with `&[]` args. The
        // typed-shim-macro work in Phase 2 may revisit this.
        if let Invocation::Strings(ref args) = invocation {
            self.task_args = args.clone();
        }

        let tracing_buffer = self.tracing_buffer.clone();
        let log_store = self.log_store.clone();
        let spawn_tx = self.spawn_tx.clone();

        // The tracing buffer was constructed with the engine-global SeqGen
        // when this TaskExecution was created. Pull a clone so we can hand
        // the same generator to the TaskContext's exec/spawn output buffer
        // below — otherwise subprocess output gets per-buffer-local seqs,
        // breaking engine-global ordering and `since_seq` subscription.
        let engine_seq_gen = {
            let buf = tracing_buffer
                .try_lock()
                .expect("tracing buffer not locked at spawn_body");
            buf.seq_gen()
        };

        let mut ctx = TaskContext::new(task.name);
        // Swap in the engine-global SeqGen so `ctx.exec()` / `ctx.spawn()`
        // subprocess output stamps with engine-global seqs (matches the
        // tracing buffer + LogStore invariant). Two places need it:
        //   1. The TaskContext's `output` buffer (used by `exec`'s pipeline
        //      and the rare direct-output paths).
        //   2. Each subprocess buffer constructed lazily inside
        //      `ctx.spawn(...)` — done by stashing the SeqGen as a field
        //      so every spawn call clones it.
        ctx.output_buffer()
            .try_lock()
            .expect("ctx output buffer uncontended at launch")
            .set_seq_gen(engine_seq_gen.clone());
        ctx.set_seq_gen(engine_seq_gen);
        ctx.set_spawn_notifier(spawn_tx);
        ctx.set_task_status(self.task_status.clone());
        ctx.set_summary(self.summary.clone());
        if let Some(ref registry) = self.registry {
            ctx.set_registry(registry.clone());
        }
        ctx.set_tracing_output(tracing_buffer.clone());
        ctx.set_task_identity(self.id, self.cancellation.clone(), self_weak);
        ctx.set_log_store(self.log_store.clone());
        let body_engine = engine.clone();
        ctx.set_engine(engine);
        ctx.set_restart_signal(self.restart_signal.clone());
        if !task.dir.is_empty() {
            ctx.set_task_dir(Some(std::path::PathBuf::from(task.dir)));
        }

        // Forward exec() output to LogStore.
        start_buffer_forwarder(ctx.output_buffer(), log_store.clone());

        // Subscribe to tracing buffer BEFORE the body runs so we don't
        // miss early entries.
        let tracing_rx = {
            let buf = tracing_buffer
                .try_lock()
                .expect("tracing buffer not locked");
            buf.subscribe()
        };
        start_tracing_forwarder(tracing_rx, log_store.clone());

        let ctx_arc = Arc::new(ctx);
        if let Ok(mut slot) = self.task_ctx.try_lock() {
            *slot = Some(ctx_arc.clone());
        }

        let task_status = self.task_status.clone();
        let body_ctx = ctx_arc.clone();
        let task_name_owned = task.name.to_string();
        let task_id = self.id;
        let tracing_buf_for_body = tracing_buffer.clone();
        let ended_at_for_body = self.ended_at.clone();
        let terminal_override_for_body = self.terminal_override.clone();

        // Mark the start of the task body. `set` is called exactly once
        // per `TaskExecution` because `spawn_body` itself runs once.
        let _ = self.started_at.set(chrono::Local::now());

        let handle: JoinHandle<TaskResult> = tokio::spawn(async move {
            use tracing::Instrument;

            let tracing_ctx = TaskTracingCtx {
                buffer: tracing_buf_for_body,
                source_label: task_name_owned,
                source_id: task_id,
            };
            let span = attach_task_tracing_ctx(tracing_ctx);
            let result = match invocation {
                Invocation::Strings(args) => {
                    async move { task.func.call(&body_ctx, &args).await }
                        .instrument(span.clone())
                        .await
                }
                Invocation::Factory(factory) => {
                    async move { factory(&body_ctx).await }
                        .instrument(span.clone())
                        .await
                }
            };

            {
                let mut s = task_status.lock().await;
                // If the cancel or timeout ladder requested a terminal
                // override before/while the body was running, honor it
                // — the user asked for a kill, even if the body
                // cooperated and returned `Ok(())`.
                if let Some(forced) = terminal_override_for_body.get() {
                    *s = forced.clone();
                } else {
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
                            // Re-enter the task's carrier span so the
                            // tracing layer routes this event to the
                            // task's log buffer (the body's `.instrument`
                            // span already ended above).
                            span.in_scope(|| {
                                tracing::error!("task failed: {}", failure.message);
                            });
                            *s = TaskStatus::Failed(failure);
                        }
                    }
                }
                let _ = ended_at_for_body.set(chrono::Local::now());
            }

            if let Some(eng) = body_engine.upgrade() {
                eng.publish_snapshot().await;
            }

            result
        });

        self.abort_handle = Some(handle.abort_handle());
        *self
            .task_handle
            .try_lock()
            .expect("task_handle slot uncontended at launch") = Some(handle);
    }

    /// Read access to the running task's `TaskContext`, if launched.
    pub async fn task_context(&self) -> Option<Arc<TaskContext>> {
        self.task_ctx.lock().await.clone()
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
            Some(h) => h.await.ok(),
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
    ///
    /// Slice 4: also publishes graph snapshots through `engine` on three
    /// process lifecycle events:
    /// - process appeared (pushed onto `processes`)
    /// - readiness flipped to `true`
    /// - process exited (detected by a 250ms signal-0 polling watcher)
    async fn monitor_spawns(
        mut spawn_rx: mpsc::UnboundedReceiver<SpawnEvent>,
        log_store: Arc<Mutex<LogStore>>,
        processes: Arc<Mutex<Vec<ProcessInfo>>>,
        engine: Weak<EngineInternals>,
    ) {
        while let Some(event) = spawn_rx.recv().await {
            let buffer = event.buffer.clone();

            // Determine initial readiness
            let has_readiness_condition = event.readiness_rx.is_some();
            let ready = !has_readiness_condition; // Ready by default if no condition

            let process_info = ProcessInfo {
                id: event.process_id,
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

            // (1) snapshot publish on process-appeared.
            if let Some(eng) = engine.upgrade() {
                tokio::spawn(async move {
                    eng.publish_snapshot().await;
                });
            }

            // Watch for readiness changes if a condition was configured.
            if let Some(mut readiness_rx) = event.readiness_rx {
                let processes_inner = processes.clone();
                let engine_ready = engine.clone();
                tokio::spawn(async move {
                    let _ = readiness_rx.wait_for(|&ready| ready).await;
                    if let Some(proc) = processes_inner.lock().await.get_mut(process_index) {
                        proc.ready = true;
                    }
                    // (2) snapshot publish on readiness flip.
                    if let Some(eng) = engine_ready.upgrade() {
                        eng.publish_snapshot().await;
                    }
                });
            }

            // (3) explicit exit watcher (slice 4) — poll signal-0 every
            // 250ms until the process is gone, then update status and
            // publish. Without this, exits only become visible to the
            // engine the next time something else calls
            // `refresh_status` (TUI render path).
            let exit_pid = event.pid;
            let exit_processes = processes.clone();
            let exit_engine = engine.clone();
            tokio::spawn(async move {
                if let Some(pid) = exit_pid {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        match signal::kill(Pid::from_raw(pid as i32), None) {
                            Ok(()) => continue, // still running
                            Err(_) => break,    // process exited
                        }
                    }
                    if let Some(proc) = exit_processes.lock().await.get_mut(process_index)
                        && proc.status == ProcessStatus::Running
                    {
                        proc.status = ProcessStatus::Done;
                    }
                    if let Some(eng) = exit_engine.upgrade() {
                        eng.publish_snapshot().await;
                    }
                }
            });

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
