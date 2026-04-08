//! Shared execution layer for task lifecycle management.
//!
//! `TaskExecution` is the unit of task execution. It owns the `TaskContext`,
//! `LogStore`, process tracking, and cleanup. All output (exec, spawn, tracing)
//! flows through the `LogStore`. UI modes (CLI, TUI, Agent) are thin consumers
//! that subscribe to the LogStore and render output in their own way.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::signal;
use nix::unistd::Pid;
use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::layer::SubscriberExt;

use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::log::store::LogStore;
use crate::task::{Registry, SpawnEvent, TaskContext, TaskDef};
use crate::tracing_layer::LogEntryLayer;
use crate::tui::output::TuiOutput;

// ============================================================
// Task and process lifecycle types
// ============================================================

/// Status of a running task.
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
    /// Process exited with a non-zero exit code or was killed by a signal.
    Failed(i32),
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
// Launch configuration
// ============================================================

/// Optional hooks passed to `TaskExecution::launch()`.
///
/// TUI-specific features (tui_wait, tui_output) are injected here.
/// CLI and Agent modes use `LaunchConfig::default()` which sets both to None.
pub struct LaunchConfig {
    /// Whether the TUI should stay open after the task completes.
    pub tui_wait: Option<Arc<AtomicBool>>,
    /// Post-TUI output staging buffer.
    pub tui_output: Option<Arc<Mutex<TuiOutput>>>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            tui_wait: None,
            tui_output: None,
        }
    }
}

// ============================================================
// TaskExecution
// ============================================================

/// A single task execution. Owns the full lifecycle from launch to cleanup.
///
/// All output (exec, spawn, tracing) flows into the `LogStore`.
/// UI modes subscribe to the LogStore and render in their own way.
pub struct TaskExecution {
    /// Name of the task being executed.
    pub task_name: String,
    /// Aggregated log store for all output.
    log_store: Arc<Mutex<LogStore>>,
    /// Current task status.
    task_status: Arc<Mutex<TaskStatus>>,
    /// Spawned processes tracked by this execution.
    processes: Arc<Mutex<Vec<ProcessInfo>>>,
    /// JoinHandle for the spawned task function.
    task_handle: Option<tokio::task::JoinHandle<()>>,
    /// The task's tracing output buffer (info!/error!/etc from the task function).
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    /// Sender for spawn events (given to the TaskContext).
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,
    /// Whether the global tracing subscriber has been installed.
    /// Shared across executions so `set_global_default` only succeeds once.
    tracing_installed: Arc<AtomicBool>,
    /// Optional shared registry for task discovery and cross-invocation.
    registry: Option<Arc<Registry>>,
}

impl TaskExecution {
    /// Create a new TaskExecution with its own LogStore.
    pub fn new() -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let log_store = Arc::new(Mutex::new(LogStore::new()));
        let processes = Arc::new(Mutex::new(Vec::new()));

        // Start the spawn monitor loop.
        let monitor_log_store = log_store.clone();
        let monitor_processes = processes.clone();
        tokio::spawn(async move {
            Self::monitor_spawns(spawn_rx, monitor_log_store, monitor_processes).await;
        });

        Self {
            task_name: String::new(),
            log_store,
            task_status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes,
            task_handle: None,
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            spawn_tx,
            tracing_installed: Arc::new(AtomicBool::new(false)),
            registry: None,
        }
    }

    /// Create a new TaskExecution that shares an existing LogStore.
    ///
    /// Used by the TUI when launching multiple tasks that should appear
    /// in the same log view.
    pub fn with_log_store(log_store: Arc<Mutex<LogStore>>) -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let processes = Arc::new(Mutex::new(Vec::new()));

        let monitor_log_store = log_store.clone();
        let monitor_processes = processes.clone();
        tokio::spawn(async move {
            Self::monitor_spawns(spawn_rx, monitor_log_store, monitor_processes).await;
        });

        Self {
            task_name: String::new(),
            log_store,
            task_status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes,
            task_handle: None,
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            spawn_tx,
            tracing_installed: Arc::new(AtomicBool::new(false)),
            registry: None,
        }
    }

    /// Set the task registry for cross-invocation and discovery.
    pub fn set_registry(&mut self, registry: Arc<Registry>) {
        self.registry = Some(registry);
    }

    /// Share a tracing_installed flag across multiple executions.
    ///
    /// `set_global_default` can only succeed once per process. When launching
    /// multiple executions (e.g. TUI picker), they should share this flag.
    pub fn set_tracing_installed(&mut self, flag: Arc<AtomicBool>) {
        self.tracing_installed = flag;
    }

    /// Launch a task. Creates the TaskContext, installs tracing, subscribes
    /// to all output buffers, and spawns the task function.
    pub fn launch(
        &mut self,
        task: &'static TaskDef,
        task_args: Vec<String>,
        config: LaunchConfig,
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
        if let Some(ref registry) = self.registry {
            ctx.set_registry(registry.clone());
        }
        if let Some(tui_wait) = config.tui_wait {
            ctx.set_tui_wait(tui_wait);
        }
        if let Some(tui_output) = config.tui_output {
            ctx.set_tui_output(tui_output);
        }
        ctx.set_tracing_output(tracing_buffer.clone());

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

        // Spawn the task function.
        let task_status = self.task_status.clone();
        let handle = tokio::spawn(async move {
            let result = task.func.call(&ctx, &task_args).await;

            let mut s = task_status.lock().await;
            match result {
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
        });

        self.task_handle = Some(handle);
    }

    /// Wait for the task function to complete.
    ///
    /// Does not stop spawned processes — call `shutdown()` for that.
    pub async fn wait(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
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
                if proc.status == ProcessStatus::Running {
                    if let Some(pgid) = proc.pgid {
                        if !pgids.contains(&pgid) {
                            pgids.push(pgid);
                        }
                    }
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

            let process_info = ProcessInfo {
                task_name: event.task_name.clone(),
                command_label: event.command_label.clone(),
                buffer: event.buffer.clone(),
                pgid: event.pgid,
                pid: event.pid,
                status: ProcessStatus::Running,
            };

            processes.lock().await.push(process_info);

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

impl Default for TaskExecution {
    fn default() -> Self {
        Self::new()
    }
}
