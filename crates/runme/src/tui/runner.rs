//! Task execution orchestrator for the TUI.
//!
//! The `TaskRunner` accepts a `TaskDef`, launches it, captures all output
//! (both tracing events from the task function and stdout/stderr from spawned
//! processes), and exposes status for the TUI to render.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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

/// The task execution orchestrator.
///
/// Owns the LogStore for aggregating all log output, tracks task status,
/// and collects information about spawned processes.
pub struct TaskRunner {
    /// Aggregated log store for all output.
    pub log_store: Arc<Mutex<LogStore>>,
    /// Current task status.
    pub status: Arc<Mutex<TaskStatus>>,
    /// Information about spawned processes.
    pub processes: Arc<Mutex<Vec<ProcessInfo>>>,
    /// The task's tracing output buffer (task function's info!/error!/etc).
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    /// Receiver for spawn events from the TaskContext.
    spawn_rx: Option<mpsc::UnboundedReceiver<SpawnEvent>>,
    /// Sender for spawn events (given to the TaskContext).
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,
    /// Whether the TUI should stay open after the task completes.
    /// Shared with the TaskContext so the task can control this at runtime.
    pub tui_wait: Arc<AtomicBool>,
    /// Post-TUI output buffer. Shared with the TaskContext so the task
    /// can stage output that gets flushed after the TUI closes.
    pub tui_output: Arc<Mutex<TuiOutput>>,
}

impl TaskRunner {
    /// Create a new TaskRunner.
    pub fn new() -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        Self {
            log_store: Arc::new(Mutex::new(LogStore::new())),
            status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes: Arc::new(Mutex::new(Vec::new())),
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            spawn_rx: Some(spawn_rx),
            spawn_tx,
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
        }
    }

    /// Create a new TaskRunner with an existing LogStore.
    pub fn with_log_store(log_store: Arc<Mutex<LogStore>>) -> Self {
        let (spawn_tx, spawn_rx) = mpsc::unbounded_channel();
        Self {
            log_store,
            status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes: Arc::new(Mutex::new(Vec::new())),
            tracing_buffer: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            spawn_rx: Some(spawn_rx),
            spawn_tx,
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
        }
    }

    /// Launch a task. This spawns the task function and a background loop that
    /// monitors spawn events and ingests output into the LogStore.
    ///
    /// Returns immediately after spawning; the task runs in the background.
    pub fn launch(&mut self, task: &'static TaskDef) {
        let tracing_buffer = self.tracing_buffer.clone();
        let log_store = self.log_store.clone();
        let status = self.status.clone();
        let processes = self.processes.clone();
        let spawn_tx = self.spawn_tx.clone();

        // Take the spawn_rx so we can move it into the background task.
        // This means launch() can only be called once.
        let spawn_rx = self
            .spawn_rx
            .take()
            .expect("launch() can only be called once per TaskRunner");

        // Install the LogEntryLayer as a scoped tracing subscriber for the task.
        let layer = LogEntryLayer::new(tracing_buffer.clone(), task.name);
        let subscriber = tracing_subscriber::registry().with(layer);

        // Create the TaskContext here so we can wire up its exec output buffer
        let mut ctx = TaskContext::new(task.name);
        ctx.set_spawn_notifier(spawn_tx);
        ctx.set_tui_wait(self.tui_wait.clone());
        ctx.set_tui_output(self.tui_output.clone());
        ctx.set_tracing_output(tracing_buffer.clone());

        // Forward exec() output (TaskContext's own buffer) to the LogStore
        let exec_log_store = log_store.clone();
        start_buffer_forwarder(ctx.output_buffer(), exec_log_store);

        // Spawn the task function.
        // Use set_global_default so spawned child tasks also inherit the subscriber.
        // (set_default is thread-local and doesn't propagate to tokio::spawn children.)
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

        // Install as global default so all threads and spawned tasks see it.
        // This must be called before any tracing macros fire.
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let _ = tracing::dispatcher::set_global_default(dispatch);

        let task_status = status.clone();
        tokio::spawn(async move {
            // Run the task function directly (no nested spawn — the global
            // subscriber covers us, and we get simpler error handling)
            let result = (task.func)(&ctx).await;

            let result = match result {
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
                    *s = TaskStatus::Failed(msg);
                }
            }
        });

        // Spawn a background task that monitors spawn events and ingests
        // process output into the LogStore.
        let monitor_log_store = log_store;
        let monitor_processes = processes;
        tokio::spawn(async move {
            Self::monitor_spawns(spawn_rx, monitor_log_store, monitor_processes).await;
        });
    }

    /// Background loop that receives spawn events and sets up output ingestion.
    async fn monitor_spawns(
        mut spawn_rx: mpsc::UnboundedReceiver<SpawnEvent>,
        log_store: Arc<Mutex<LogStore>>,
        processes: Arc<Mutex<Vec<ProcessInfo>>>,
    ) {
        while let Some(event) = spawn_rx.recv().await {
            let buffer = event.buffer.clone();

            // Record the process for display
            processes.lock().await.push(ProcessInfo {
                task_name: event.task_name.clone(),
                command_label: event.command_label.clone(),
                buffer: event.buffer.clone(),
                pgid: event.pgid,
                pid: event.pid,
                status: ProcessStatus::Running,
            });

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
    use crate::task::TaskDef;
    use std::future::Future;
    use std::pin::Pin;

    fn success_task(
        ctx: &TaskContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
        Box::pin(async move {
            tracing::info!("hello from task {}", ctx.name);
            Ok(())
        })
    }

    fn failing_task(
        _ctx: &TaskContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
        Box::pin(async move { Err(TaskError::from_display("intentional failure")) })
    }

    fn spawning_task(
        ctx: &TaskContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
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

        depends_on: &[],
        func: success_task,
    };

    static FAILING_TASK: TaskDef = TaskDef {
        name: "failing",
        description: Some("A failing task"),
        group: "",

        depends_on: &[],
        func: failing_task,
    };

    static SPAWNING_TASK: TaskDef = TaskDef {
        name: "spawning",
        description: Some("A task that spawns a process"),
        group: "",

        depends_on: &[],
        func: spawning_task,
    };

    #[tokio::test]
    async fn test_runner_success_transitions_to_done() {
        let mut runner = TaskRunner::new();
        runner.launch(&SUCCESS_TASK);

        // Wait for the task to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let status = runner.status.lock().await;
        assert_eq!(*status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn test_runner_failure_transitions_to_failed() {
        let mut runner = TaskRunner::new();
        runner.launch(&FAILING_TASK);

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
        runner.launch(&SPAWNING_TASK);

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

        runner.launch(&SPAWNING_TASK);

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
}
