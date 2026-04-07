//! Task execution orchestrator for the TUI.
//!
//! The `TaskRunner` accepts a `TaskDef`, launches it, captures all output
//! (both tracing events from the task function and stdout/stderr from spawned
//! processes), and exposes status for the TUI to render.

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tracing_subscriber::layer::SubscriberExt;

use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::log::store::LogStore;
use crate::task::{SpawnEvent, TaskContext, TaskDef};
use crate::tracing_layer::LogEntryLayer;

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

/// Information about a spawned process, for the TUI to display.
pub struct ProcessInfo {
    pub task_name: String,
    pub buffer: Arc<Mutex<OutputBuffer>>,
    pub pgid: Option<i32>,
    pub pid: Option<u32>,
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

        // Start forwarding tracing output to the LogStore in real time.
        start_tracing_forwarder(tracing_buffer.clone(), log_store.clone());

        // Install the LogEntryLayer as a scoped tracing subscriber for the task.
        let layer = LogEntryLayer::new(tracing_buffer.clone(), task.name);
        let subscriber = tracing_subscriber::registry().with(layer);

        // Create the TaskContext here so we can wire up its exec output buffer
        let mut ctx = TaskContext::new(task.name);
        ctx.set_spawn_notifier(spawn_tx);

        // Forward exec() output (TaskContext's own buffer) to the LogStore
        let exec_log_store = log_store.clone();
        start_buffer_forwarder(ctx.output_buffer(), exec_log_store);

        // Spawn the task function.
        // Use set_global_default so spawned child tasks also inherit the subscriber.
        // (set_default is thread-local and doesn't propagate to tokio::spawn children.)
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
                buffer: event.buffer.clone(),
                pgid: event.pgid,
                pid: event.pid,
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
fn start_tracing_forwarder(
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    log_store: Arc<Mutex<LogStore>>,
) {
    tokio::spawn(async move {
        let mut rx = {
            let buf = tracing_buffer.lock().await;
            buf.subscribe()
        };
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
        watch: None,
        depends_on: &[],
        func: success_task,
    };

    static FAILING_TASK: TaskDef = TaskDef {
        name: "failing",
        description: Some("A failing task"),
        group: "",
        watch: None,
        depends_on: &[],
        func: failing_task,
    };

    static SPAWNING_TASK: TaskDef = TaskDef {
        name: "spawning",
        description: Some("A task that spawns a process"),
        group: "",
        watch: None,
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
        assert_eq!(procs[0].task_name, "spawning");
    }

    #[tokio::test]
    async fn test_runner_log_store_receives_entries() {
        let mut runner = TaskRunner::new();

        // Subscribe to log store before launching
        let mut rx = runner.subscribe().await;

        // Also start the tracing forwarder so tracing output reaches the log store live
        start_tracing_forwarder(runner.tracing_buffer.clone(), runner.log_store.clone());

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
        assert!(
            !entries.is_empty(),
            "expected log entries but got none"
        );
    }

    #[tokio::test]
    async fn test_runner_starts_in_setup() {
        let runner = TaskRunner::new();
        let status = runner.status.lock().await;
        assert_eq!(*status, TaskStatus::Setup);
    }
}
