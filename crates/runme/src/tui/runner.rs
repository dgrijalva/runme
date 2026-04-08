//! Task execution orchestrator for the TUI.
//!
//! The `TaskRunner` accepts one or more `TaskDef`s, launches them, captures all
//! output (both tracing events from task functions and stdout/stderr from spawned
//! processes), and exposes status for the TUI to render.
//!
//! Multiple tasks can run concurrently via `TaskSession`s. Each session tracks
//! its own status and process list, while sharing a single `LogStore`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::task::Registry;

use nix::sys::signal;
use nix::unistd::Pid;
use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::layer::SubscriberExt;

use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::log::store::LogStore;
use crate::task::{SpawnEvent, TaskContext, TaskDef};
use crate::tracing_layer::LogEntryLayer;

use super::output::TuiOutput;

/// Status of the running task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task function is still executing (spawning processes, doing setup).
    Setup,
    /// Task function returned Ok, but spawned processes are still running.
    Ready,
    /// Task function returned Ok and no processes remain.
    Done,
    /// Task function returned an error or panicked.
    Failed(String),
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

/// Information about a spawned process, for the TUI to display.
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
            // kill(pid, 0) checks if process exists without sending a signal
            match signal::kill(Pid::from_raw(pid as i32), None) {
                Ok(()) => {} // still running
                Err(_) => {
                    // Process no longer exists — mark as Done
                    // We can't easily distinguish Done vs Failed without waitpid,
                    // but for display purposes this is good enough.
                    self.status = ProcessStatus::Done;
                }
            }
        }
    }

    /// Get a short display label for this process.
    /// Uses the first token of the command label (e.g., "echo" from "echo hello").
    pub fn display_name(&self) -> &str {
        &self.command_label
    }
}

/// Mapping from task name to that session's process list. Used by the
/// monitor loop to route spawn events to the correct session.
type SessionProcessMap = Arc<Mutex<Vec<(String, Arc<Mutex<Vec<ProcessInfo>>>)>>>;

/// A session representing a single launched task within the runner.
///
/// Each call to `TaskRunner::launch()` creates a new session with its own
/// status and process list. All sessions share the runner's `LogStore`.
pub struct TaskSession {
    /// Unique session ID (monotonically increasing within the runner).
    pub id: usize,
    /// Name of the task being executed.
    pub task_name: String,
    /// Current status of this task.
    pub status: Arc<Mutex<TaskStatus>>,
    /// Processes spawned by this task.
    pub processes: Arc<Mutex<Vec<ProcessInfo>>>,
}

/// The task execution orchestrator.
///
/// Owns the LogStore for aggregating all log output, tracks task sessions,
/// and collects information about spawned processes. Supports launching
/// multiple concurrent tasks, each tracked as a `TaskSession`.
pub struct TaskRunner {
    /// Aggregated log store for all output.
    pub log_store: Arc<Mutex<LogStore>>,
    /// Current task status (first session, for backward compatibility).
    pub status: Arc<Mutex<TaskStatus>>,
    /// Information about spawned processes (first session, for backward compatibility).
    pub processes: Arc<Mutex<Vec<ProcessInfo>>>,
    /// The task's tracing output buffer (task function's info!/error!/etc).
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    /// Sender for spawn events (given to each TaskContext).
    /// The receiver is consumed once by the monitor loop started in `new()`.
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,
    /// Whether the TUI should stay open after the task completes.
    /// Shared with the TaskContext so the task can control this at runtime.
    pub tui_wait: Arc<AtomicBool>,
    /// Post-TUI output buffer. Shared with the TaskContext so the task
    /// can stage output that gets flushed after the TUI closes.
    pub tui_output: Arc<Mutex<TuiOutput>>,
    /// All task sessions, in launch order.
    pub sessions: Vec<TaskSession>,
    /// Next session ID counter.
    next_session_id: usize,
    /// Whether the global tracing subscriber has been installed.
    /// `set_global_default` can only succeed once per process.
    tracing_installed: Arc<AtomicBool>,
    /// Shared list of all session process arcs, used by the monitor loop
    /// to route spawn events to the correct session.
    session_processes: SessionProcessMap,
    /// Optional shared registry for task discovery and cross-invocation.
    registry: Option<Arc<Registry>>,
}

impl TaskRunner {
    /// Create a new TaskRunner.
    pub fn new() -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let log_store = Arc::new(Mutex::new(LogStore::new()));
        let session_processes = Arc::new(Mutex::new(Vec::new()));

        // Start the monitor loop once. It runs for the lifetime of the runner
        // and routes incoming SpawnEvents to the appropriate session's process list.
        let monitor_log_store = log_store.clone();
        let monitor_session_procs = session_processes.clone();
        tokio::spawn(async move {
            Self::monitor_spawns(spawn_rx, monitor_log_store, monitor_session_procs).await;
        });

        Self {
            log_store,
            status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes: Arc::new(Mutex::new(Vec::new())),
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            spawn_tx,
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
            sessions: Vec::new(),
            next_session_id: 0,
            tracing_installed: Arc::new(AtomicBool::new(false)),
            session_processes,
            registry: None,
        }
    }

    /// Set the task registry for cross-invocation and discovery.
    pub fn set_registry(&mut self, registry: Arc<Registry>) {
        self.registry = Some(registry);
    }

    /// Create a new TaskRunner with an existing LogStore.
    pub fn with_log_store(log_store: Arc<Mutex<LogStore>>) -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        let session_processes = Arc::new(Mutex::new(Vec::new()));

        // Start the monitor loop once.
        let monitor_log_store = log_store.clone();
        let monitor_session_procs = session_processes.clone();
        tokio::spawn(async move {
            Self::monitor_spawns(spawn_rx, monitor_log_store, monitor_session_procs).await;
        });

        Self {
            log_store,
            status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes: Arc::new(Mutex::new(Vec::new())),
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            spawn_tx,
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
            sessions: Vec::new(),
            next_session_id: 0,
            tracing_installed: Arc::new(AtomicBool::new(false)),
            session_processes,
            registry: None,
        }
    }

    /// Launch a task. This spawns the task function and sets up output ingestion
    /// into the shared LogStore.
    ///
    /// Can be called multiple times to launch concurrent tasks. Each call creates
    /// a new `TaskSession`. Returns the session ID.
    pub fn launch(&mut self, task: &'static TaskDef, task_args: Vec<String>) -> usize {
        let session_id = self.next_session_id;
        self.next_session_id += 1;

        // Create per-session status and process list
        let session_status = Arc::new(Mutex::new(TaskStatus::Setup));
        let session_processes = Arc::new(Mutex::new(Vec::new()));

        // For the first session, wire up the backward-compat fields
        if session_id == 0 {
            self.status = session_status.clone();
            self.processes = session_processes.clone();
        }

        // Register this session's process list with the monitor loop
        {
            let sp = self.session_processes.clone();
            let task_name = task.name.to_string();
            let procs = session_processes.clone();
            // Use try_lock since we're not in an async context. The monitor
            // loop only holds the lock briefly so this should succeed.
            if let Ok(mut guard) = sp.try_lock() {
                guard.push((task_name.clone(), procs));
            }
        }

        let session = TaskSession {
            id: session_id,
            task_name: task.name.to_string(),
            status: session_status.clone(),
            processes: session_processes,
        };
        self.sessions.push(session);

        let tracing_buffer = self.tracing_buffer.clone();
        let log_store = self.log_store.clone();
        let spawn_tx = self.spawn_tx.clone();

        // Install the LogEntryLayer as a scoped tracing subscriber for the task.
        let layer = LogEntryLayer::new(tracing_buffer.clone(), task.name);
        let subscriber = tracing_subscriber::registry().with(layer);

        // Create the TaskContext here so we can wire up its exec output buffer
        let mut ctx = TaskContext::new(task.name);
        ctx.set_spawn_notifier(spawn_tx);
        ctx.set_tui_wait(self.tui_wait.clone());
        ctx.set_tui_output(self.tui_output.clone());
        ctx.set_tracing_output(tracing_buffer.clone());
        if let Some(ref registry) = self.registry {
            ctx.set_registry(registry.clone());
        }

        // Forward exec() output (TaskContext's own buffer) to the LogStore
        let exec_log_store = log_store.clone();
        start_buffer_forwarder(ctx.output_buffer(), exec_log_store);

        // Subscribe to the tracing buffer BEFORE installing the subscriber
        // and spawning the task. Broadcast only delivers messages sent after
        // subscribe, so we must subscribe first to avoid missing early entries.
        let tracing_rx = {
            let buf = tracing_buffer
                .try_lock()
                .expect("tracing buffer not locked");
            buf.subscribe()
        };
        start_tracing_forwarder(tracing_rx, log_store.clone());

        // Install the global default tracing subscriber only once.
        // set_global_default can only succeed once per process.
        if !self.tracing_installed.swap(true, Ordering::SeqCst) {
            let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
            let _ = tracing::dispatcher::set_global_default(dispatch);
        }

        let task_status = session_status;
        tokio::spawn(async move {
            // Run the task function directly (no nested spawn — the global
            // subscriber covers us, and we get simpler error handling)
            let result = task.func.call(&ctx, &task_args).await;

            let result: Result<(), String> = match result {
                Ok(()) => Ok(()),
                Err(task_err) => Err(task_err.to_string()),
            };

            // Update status based on result
            let mut s = task_status.lock().await;
            match result {
                Ok(()) => {
                    *s = TaskStatus::Done;
                }
                Err(msg) => {
                    tracing::error!("task failed: {}", msg);
                    *s = TaskStatus::Failed(msg);
                }
            }
        });

        session_id
    }

    /// Background loop that receives spawn events and sets up output ingestion.
    ///
    /// Routes each spawn event to the correct session's process list based on
    /// `task_name`. Falls back to adding to all sessions if no match is found.
    async fn monitor_spawns(
        mut spawn_rx: mpsc::UnboundedReceiver<SpawnEvent>,
        log_store: Arc<Mutex<LogStore>>,
        session_processes: SessionProcessMap,
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

            // Route the process to the appropriate session's process list.
            // SpawnEvent.task_name is the command label (source name), not the
            // task name. We add it to the first session whose task_name matches
            // the original task, or fall back to the first session.
            {
                let sessions = session_processes.lock().await;
                if sessions.len() == 1 {
                    // Fast path: single session, no ambiguity
                    sessions[0].1.lock().await.push(process_info);
                } else if !sessions.is_empty() {
                    // For now, add to the last (most recently launched) session.
                    // In the future, SpawnEvent could carry a session ID for
                    // precise routing.
                    sessions.last().unwrap().1.lock().await.push(process_info);
                }
            }

            // Subscribe to the process's output buffer and forward entries
            // to the LogStore.
            let store = log_store.clone();
            let mut rx = {
                let buf = buffer.lock().await;
                // Ingest any entries already in the buffer
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Missed some entries due to buffer overflow; continue
                            continue;
                        }
                    }
                }
            });
        }
    }

    /// Subscribe to the LogStore's broadcast channel for new entries.
    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry> {
        self.log_store.lock().await.subscribe()
    }

    /// Shut down all spawned processes across all sessions.
    ///
    /// Sends SIGTERM to each process group, waits for the grace period,
    /// then sends SIGKILL to any survivors. Should be called when the TUI exits.
    pub async fn shutdown(&self, timeout: std::time::Duration) {
        let mut pgids: Vec<i32> = Vec::new();

        for session in &self.sessions {
            let procs = session.processes.lock().await;
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

        // Grace period
        tokio::time::sleep(timeout).await;

        // SIGKILL any survivors
        for &pgid in &pgids {
            let _ = signal::killpg(Pid::from_raw(pgid), Some(signal::Signal::SIGKILL));
        }
    }
}

/// Forward entries from any OutputBuffer to the LogStore via broadcast subscription.
fn start_buffer_forwarder(
    buffer: &tokio::sync::Mutex<OutputBuffer>,
    log_store: Arc<Mutex<LogStore>>,
) {
    // Subscribe synchronously while we have a reference to the buffer's mutex.
    // We use try_lock here since we're not in an async context.
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
/// The receiver must be created BEFORE launching the task to avoid
/// missing early entries (broadcast only delivers messages sent after subscribe).
fn start_tracing_forwarder(
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

impl Default for TaskRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TaskError;
    use crate::task::{TaskDef, TaskFnKind};
    use std::future::Future;
    use std::pin::Pin;

    fn no_arg_metadata() -> Option<clap::Command> {
        None
    }

    fn success_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!("hello from task {}", ctx.name);
            Ok(())
        })
    }

    fn failing_task<'a>(
        _ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        Box::pin(async move { Err(TaskError::from_display("intentional failure")) })
    }

    fn spawning_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!("about to spawn");
            let _handle = ctx.spawn("echo spawned_output").await?;
            // Give the process time to produce output
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(())
        })
    }

    static SUCCESS_TASK: TaskDef = TaskDef {
        name: "success",
        description: Some("A successful task"),
        group: "",
        func: TaskFnKind::Static(success_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static FAILING_TASK: TaskDef = TaskDef {
        name: "failing",
        description: Some("A failing task"),
        group: "",
        func: TaskFnKind::Static(failing_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static SPAWNING_TASK: TaskDef = TaskDef {
        name: "spawning",
        description: Some("A task that spawns a process"),
        group: "",
        func: TaskFnKind::Static(spawning_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    #[tokio::test]
    async fn test_runner_success_transitions_to_done() {
        let mut runner = TaskRunner::new();
        runner.launch(&SUCCESS_TASK, Vec::new());

        // Wait for the task to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let status = runner.status.lock().await;
        assert_eq!(*status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn test_runner_failure_transitions_to_failed() {
        let mut runner = TaskRunner::new();
        runner.launch(&FAILING_TASK, Vec::new());

        // Wait for the task to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let status = runner.status.lock().await;
        match &*status {
            TaskStatus::Failed(msg) => {
                assert!(msg.contains("intentional failure"));
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_runner_spawn_notification() {
        let mut runner = TaskRunner::new();
        runner.launch(&SPAWNING_TASK, Vec::new());

        // Wait for the task to spawn and complete
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        let procs = runner.processes.lock().await;
        assert!(
            !procs.is_empty(),
            "expected at least one spawned process, got {}",
            procs.len()
        );
        // The task_name on ProcessInfo comes from the ProcessHandle, which
        // now uses the command label as its source identifier.
        assert!(
            !procs[0].task_name.is_empty(),
            "process should have a task_name"
        );
    }

    #[tokio::test]
    async fn test_runner_log_store_receives_entries() {
        let mut runner = TaskRunner::new();

        // Subscribe to log store before launching
        let mut rx = runner.subscribe().await;

        // Also start the tracing forwarder so tracing output reaches the log store live
        let tracing_rx = {
            let buf = runner.tracing_buffer.lock().await;
            buf.subscribe()
        };
        start_tracing_forwarder(tracing_rx, runner.log_store.clone());

        runner.launch(&SPAWNING_TASK, Vec::new());

        // Collect entries for a bit
        let mut entries = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(entry) => entries.push(entry),
                        Err(_) => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }

        // Should have received at least the tracing output and the spawned process output
        assert!(!entries.is_empty(), "expected log entries but got none");
    }

    #[tokio::test]
    async fn test_runner_starts_in_setup() {
        let runner = TaskRunner::new();
        let status = runner.status.lock().await;
        assert_eq!(*status, TaskStatus::Setup);
    }

    #[tokio::test]
    async fn test_runner_creates_session_on_launch() {
        let mut runner = TaskRunner::new();
        assert!(runner.sessions.is_empty());

        let session_id = runner.launch(&SUCCESS_TASK, Vec::new());
        assert_eq!(session_id, 0);
        assert_eq!(runner.sessions.len(), 1);
        assert_eq!(runner.sessions[0].task_name, "success");
        assert_eq!(runner.sessions[0].id, 0);
    }

    #[tokio::test]
    async fn test_runner_first_session_maps_to_compat_fields() {
        let mut runner = TaskRunner::new();
        runner.launch(&SUCCESS_TASK, Vec::new());

        // Wait for task completion
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The backward-compat `status` field should point to the first session's status
        let compat_status = runner.status.lock().await.clone();
        let session_status = runner.sessions[0].status.lock().await.clone();
        assert_eq!(compat_status, session_status);
    }
}
