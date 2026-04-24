//! TUI task runner — thin wrapper over the shared execution layer.
//!
//! `TaskRunner` manages one or more `TaskExecution`s for the TUI,
//! providing session tracking for the sidebar and shared TUI hooks
//! (tui_wait, tui_output).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Mutex;

use crate::execution::{LaunchConfig, TaskExecution};
use crate::log::LogEntry;
use crate::log::store::LogStore;
use crate::task::{Registry, TaskDef};

use super::output::TuiOutput;

// Re-export execution types so existing TUI imports (sidebar, event, etc.) keep working.
pub use crate::execution::{ProcessInfo, ProcessStatus, TaskStatus};

/// A session representing a single launched task within the runner.
///
/// Each call to `TaskRunner::launch()` creates a new session backed by
/// a `TaskExecution`. The session provides TUI-friendly accessors.
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

/// The TUI task runner. Wraps the shared execution layer with TUI-specific
/// session management and hooks.
pub struct TaskRunner {
    /// Shared LogStore across all executions.
    pub log_store: Arc<Mutex<LogStore>>,
    /// Current task status (first session, for backward compatibility).
    pub status: Arc<Mutex<TaskStatus>>,
    /// Information about spawned processes (first session, for backward compatibility).
    pub processes: Arc<Mutex<Vec<ProcessInfo>>>,
    /// Whether the TUI should stay open after the task completes.
    pub tui_wait: Arc<AtomicBool>,
    /// Post-TUI output buffer.
    pub tui_output: Arc<Mutex<TuiOutput>>,
    /// All task sessions, in launch order.
    pub sessions: Vec<TaskSession>,
    /// The underlying executions.
    executions: Vec<TaskExecution>,
    /// Next session ID counter.
    next_session_id: usize,
    /// Shared tracing_installed flag across executions.
    tracing_installed: Arc<AtomicBool>,
    /// Optional shared registry for task discovery and cross-invocation.
    registry: Option<Arc<Registry>>,
}

impl TaskRunner {
    /// Create a new TaskRunner.
    pub fn new() -> Self {
        Self {
            log_store: Arc::new(Mutex::new(LogStore::new())),
            status: Arc::new(Mutex::new(TaskStatus::Setup)),
            processes: Arc::new(Mutex::new(Vec::new())),
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
            sessions: Vec::new(),
            executions: Vec::new(),
            next_session_id: 0,
            tracing_installed: Arc::new(AtomicBool::new(false)),
            registry: None,
        }
    }

    /// Set the task registry for cross-invocation and discovery.
    pub fn set_registry(&mut self, registry: Arc<Registry>) {
        self.registry = Some(registry);
    }

    /// Launch a task. Creates a `TaskExecution`, wires up TUI hooks,
    /// and tracks it as a session.
    ///
    /// Returns the session ID.
    pub fn launch(&mut self, task: &'static TaskDef, task_args: Vec<String>) -> usize {
        let session_id = self.next_session_id;
        self.next_session_id += 1;

        // Create a TaskExecution sharing this runner's LogStore.
        let mut exec = TaskExecution::with_log_store(self.log_store.clone());
        if let Some(ref registry) = self.registry {
            exec.set_registry(registry.clone());
        }
        exec.set_tracing_installed(self.tracing_installed.clone());

        // Launch with TUI hooks.
        let config = LaunchConfig {
            tui_wait: Some(self.tui_wait.clone()),
            tui_output: Some(self.tui_output.clone()),
        };
        exec.launch(task, task_args, config);

        // Create a session that points to this execution's state.
        let session = TaskSession {
            id: session_id,
            task_name: exec.task_name.clone(),
            status: exec.task_status().clone(),
            processes: exec.processes().clone(),
        };

        // For the first session, wire up the backward-compat fields.
        if session_id == 0 {
            self.status = session.status.clone();
            self.processes = session.processes.clone();
        }

        self.sessions.push(session);
        self.executions.push(exec);

        session_id
    }

    /// Subscribe to the LogStore's broadcast channel for new entries.
    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry> {
        self.log_store.lock().await.subscribe()
    }

    /// Shut down all spawned processes across all executions.
    pub async fn shutdown(&self, timeout: std::time::Duration) {
        for exec in &self.executions {
            exec.shutdown(timeout).await;
        }
    }
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
    use crate::execution::start_tracing_forwarder;
    use crate::task::{TaskContext, TaskDef, TaskFnKind};
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
            TaskStatus::Failed(failure) => {
                assert!(failure.message.contains("intentional failure"));
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
