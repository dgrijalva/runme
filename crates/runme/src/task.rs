use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;

use crate::cmd::Cmd;
use crate::error::TaskError;
use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::process::{self, Output, ProcessError, ProcessHandle, ProcessResult};
use crate::tui::output::{TuiOutput, TuiOutputHandle};
use crate::watch::{self, Watch, WatchInfo, WatchKind};

/// The type of async task functions.
///
/// Task functions are `async fn(&TaskContext) -> Result<(), TaskError>` — this
/// type alias represents that as a function pointer returning a boxed future.
pub type TaskFn =
    fn(&TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>>;

/// Task metadata — what the macro extracts and registers.
///
/// Uses `&'static str` for name/description so that instances can be
/// constructed in static context and satisfy `Send + Sync + 'static`
/// (required by `inventory`).
pub struct TaskDef {
    pub name: &'static str,
    pub description: Option<&'static str>,
    pub group: &'static str,
    pub depends_on: &'static [&'static str],
    pub func: TaskFn,
}

// Safety: TaskDef contains only 'static references and function pointers,
// which are inherently Send + Sync.
unsafe impl Send for TaskDef {}
unsafe impl Sync for TaskDef {}

// inventory requires Collect impl
inventory::collect!(TaskDef);

/// Runtime context passed to task functions.
///
/// Provides process execution, output capture, and lifecycle management.
pub struct TaskContext {
    pub name: String,
    output: Arc<Mutex<OutputBuffer>>,
    /// The tracing output buffer (info!/error!/etc from the task function).
    /// Injected by TaskRunner; None when running outside the TUI (e.g., tests).
    tracing_output: Option<Arc<Mutex<OutputBuffer>>>,
    /// Process group IDs of all processes spawned through this context.
    /// Used by `stop_all()` to signal every spawned process group.
    spawned_pgids: Mutex<Vec<i32>>,
    /// Optional channel to notify an observer (e.g., the TUI runner) when a
    /// process is spawned. The receiver gets the `ProcessHandle`'s output buffer
    /// and task name so it can register the process for display.
    spawn_tx: Option<mpsc::UnboundedSender<SpawnEvent>>,
    /// Whether the TUI should stay open after the task completes.
    /// Default: true (TUI stays open). Set to false via `ctx.tui_wait(false)`
    /// to auto-exit on task completion.
    tui_wait: Arc<AtomicBool>,
    /// Post-TUI output buffer. Entries staged here are flushed to real
    /// stdout/stderr after the TUI closes.
    tui_output: Arc<Mutex<TuiOutput>>,
    /// All watches registered through this context, for TUI visibility.
    watches: Arc<std::sync::Mutex<Vec<Arc<std::sync::Mutex<WatchInfo>>>>>,
    /// Working directory for file watches. Defaults to the current directory.
    watch_dir: PathBuf,
}

/// Event emitted when a process is spawned through a TaskContext.
///
/// Contains the information the TUI runner needs to track and display the process.
pub struct SpawnEvent {
    /// The output buffer for the spawned process.
    pub buffer: Arc<Mutex<OutputBuffer>>,
    /// The task name associated with this process.
    pub task_name: String,
    /// The process group ID, if available.
    pub pgid: Option<i32>,
    /// The process ID, if available.
    pub pid: Option<u32>,
    /// Human-readable label for the command (e.g., "echo hello", "npm run dev").
    pub command_label: String,
}

impl TaskContext {
    /// Create a new TaskContext for the named task.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            output: Arc::new(Mutex::new(OutputBuffer::new(10_000))),
            tracing_output: None,
            spawned_pgids: Mutex::new(Vec::new()),
            spawn_tx: None,
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
            watches: Arc::new(std::sync::Mutex::new(Vec::new())),
            watch_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// Create a new TaskContext with a custom buffer capacity.
    pub fn with_capacity(name: impl Into<String>, capacity: usize) -> Self {
        Self {
            name: name.into(),
            output: Arc::new(Mutex::new(OutputBuffer::new(capacity))),
            tracing_output: None,
            spawned_pgids: Mutex::new(Vec::new()),
            spawn_tx: None,
            tui_wait: Arc::new(AtomicBool::new(true)),
            tui_output: Arc::new(Mutex::new(TuiOutput::new())),
            watches: Arc::new(std::sync::Mutex::new(Vec::new())),
            watch_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// Set a channel sender that will be notified whenever `spawn()` is called.
    ///
    /// The TUI runner uses this to learn about new processes and register their
    /// output buffers for display.
    pub fn set_spawn_notifier(&mut self, tx: mpsc::UnboundedSender<SpawnEvent>) {
        self.spawn_tx = Some(tx);
    }

    /// Access the task's output buffer (contains output from `exec()` calls).
    ///
    /// The TUI runner uses this to forward exec output to the LogStore.
    pub fn output_buffer(&self) -> &Mutex<OutputBuffer> {
        &self.output
    }

    /// Control whether the TUI stays open after the task completes.
    ///
    /// By default, the TUI stays open (`true`). Set to `false` to auto-exit
    /// when the task returns. This is mutable — the task can change its mind:
    ///
    /// ```ignore
    /// ctx.tui_wait(false);
    /// let result = ctx.exec("cargo install --path .").await?;
    /// if !result.success() {
    ///     ctx.tui_wait(true);  // stay open on failure
    /// }
    /// ```
    pub fn tui_wait(&self, wait: bool) {
        self.tui_wait.store(wait, Ordering::Relaxed);
    }

    /// Get a handle to the TUI output buffer for staging post-TUI output.
    ///
    /// The returned handle supports chaining:
    /// ```ignore
    /// ctx.tui_output().stderr().append(result.output()).await;
    /// ctx.tui_output().stdout().write("done!\n").await;
    /// ```
    pub fn tui_output(&self) -> TuiOutputHandle {
        TuiOutputHandle::new(self.tui_output.clone())
    }

    /// Get an `Output` handle wrapping the task's tracing output buffer.
    ///
    /// This is the output from `info!()`, `error!()`, etc. called within
    /// the task function. Useful for staging task tracing output to post-TUI:
    ///
    /// ```ignore
    /// ctx.tui_output().stderr().subscribe(&ctx.task_output()).await;
    /// ```
    ///
    /// Falls back to the exec output buffer if no tracing buffer was injected
    /// (e.g., in tests or non-TUI mode).
    pub fn task_output(&self) -> Output {
        Output(
            self.tracing_output
                .clone()
                .unwrap_or_else(|| self.output.clone()),
        )
    }

    /// Get the `tui_wait` flag as a shared Arc for external observation.
    ///
    /// The TUI event loop uses this to decide whether to auto-exit.
    pub fn tui_wait_flag(&self) -> Arc<AtomicBool> {
        self.tui_wait.clone()
    }

    /// Get the shared TUI output Arc for external use (e.g., flushing on shutdown).
    pub fn tui_output_arc(&self) -> Arc<Mutex<TuiOutput>> {
        self.tui_output.clone()
    }

    /// Set the tui_wait Arc (used by TaskRunner to inject a shared flag).
    pub fn set_tui_wait(&mut self, flag: Arc<AtomicBool>) {
        self.tui_wait = flag;
    }

    /// Set the tui_output Arc (used by TaskRunner to inject a shared buffer).
    pub fn set_tui_output(&mut self, output: Arc<Mutex<TuiOutput>>) {
        self.tui_output = output;
    }

    /// Set the tracing output buffer (used by TaskRunner to inject the
    /// buffer that LogEntryLayer writes to).
    pub fn set_tracing_output(&mut self, buffer: Arc<Mutex<OutputBuffer>>) {
        self.tracing_output = Some(buffer);
    }

    /// Run a command and wait for it to complete. Captures output.
    ///
    /// Returns a `ProcessResult` for any exit code. Use `.ok()?` to propagate
    /// non-zero exit codes as errors, or inspect `.success()` and `.exit_code()`.
    ///
    /// Accepts a `Cmd`, `&str`, or `String`. Strings are treated as shell commands.
    pub async fn exec(&self, command: impl Into<Cmd>) -> Result<ProcessResult, ProcessError> {
        let mut buffer = self.output.lock().await;
        process::exec(command, &self.name, &mut buffer).await
    }

    /// Spawn a long-running command. Returns a handle for monitoring/control.
    ///
    /// The process group ID is tracked internally so that `stop_all()` can
    /// shut down every process spawned through this context.
    ///
    /// Accepts a `Cmd`, `&str`, or `String`. Strings are treated as shell commands.
    pub async fn spawn(&self, command: impl Into<Cmd>) -> Result<ProcessHandle, ProcessError> {
        let cmd: Cmd = command.into();
        let command_label = cmd.display_label();
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(10_000)));
        // Use the command label as the source name so each process gets a
        // distinct source in the log viewer (rather than all sharing the task name).
        let handle = process::spawn(cmd, &command_label, buffer).await?;

        // Track the process group so stop_all() can signal it
        if let Some(pgid) = handle.pgid() {
            self.spawned_pgids.lock().await.push(pgid);
        } else if let Some(pid) = handle.pid() {
            self.spawned_pgids.lock().await.push(pid as i32);
        }

        // Notify the TUI runner (if connected) about the new process
        if let Some(tx) = &self.spawn_tx {
            let _ = tx.send(SpawnEvent {
                buffer: handle.output().0.clone(),
                task_name: handle.task_name().to_string(),
                pgid: handle.pgid(),
                pid: handle.pid(),
                command_label,
            });
        }

        Ok(handle)
    }

    /// Stop all processes spawned through this context.
    ///
    /// Sends SIGTERM to each process group, waits for the timeout, then
    /// sends SIGKILL to any that are still alive.
    pub async fn stop_all(&self, timeout: std::time::Duration) {
        let pgids = self.spawned_pgids.lock().await;
        for &pgid in pgids.iter() {
            // Send SIGTERM to the process group
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                Some(nix::sys::signal::Signal::SIGTERM),
            );
        }

        // Wait for the grace period
        tokio::time::sleep(timeout).await;

        // SIGKILL any survivors
        for &pgid in pgids.iter() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                Some(nix::sys::signal::Signal::SIGKILL),
            );
        }
    }

    /// Watch files matching a glob pattern.
    ///
    /// Returns a `Watch<Vec<PathBuf>>` that yields batches of changed file paths
    /// each time matching files are created, modified, or removed. Events are
    /// debounced (coalesced over a short window) so rapid changes produce a
    /// single batch.
    ///
    /// ```ignore
    /// let mut w = ctx.watch("src/**/*.rs").label("rust sources");
    /// loop {
    ///     ctx.exec("cargo build").await.ok()?;
    ///     w.next().await;
    /// }
    /// ```
    pub fn watch(&self, pattern: &str) -> Watch<Vec<PathBuf>> {
        let (rx, info, _actual_dir) = watch::start_file_watcher(pattern, self.watch_dir.clone())
            .expect("failed to start file watcher");

        // Register for TUI visibility
        if let Ok(mut watches) = self.watches.lock() {
            watches.push(info.clone());
        }

        Watch::new(rx, info)
    }

    /// Watch files with a custom filter/map function.
    ///
    /// The `pattern` determines which directory to watch (the non-glob prefix
    /// is resolved relative to the RUNME.rs file's location). All matching
    /// filesystem events are collected, debounced, and passed to `filter_fn`.
    /// If it returns `Some(value)`, that value is delivered via `.next()`.
    /// If `None`, the event batch is discarded and the watch keeps waiting.
    ///
    /// ```ignore
    /// let mut w = ctx.watch_with("src/**/*.rs", |changed| {
    ///     let rs = glob_filter("**/*.rs", changed);
    ///     let toml = glob_filter("**/Cargo.toml", changed);
    ///     if rs.is_empty() && toml.is_empty() { None }
    ///     else { Some((rs, toml)) }
    /// });
    /// ```
    pub fn watch_with<F, T>(&self, pattern: &str, filter_fn: F) -> Watch<T>
    where
        F: Fn(&[PathBuf]) -> Option<T> + Send + 'static,
        T: Send + 'static,
    {
        let (rx, info, _actual_dir) =
            watch::start_filtered_watcher(pattern, self.watch_dir.clone(), filter_fn)
                .expect("failed to start filtered watcher");

        if let Ok(mut watches) = self.watches.lock() {
            watches.push(info.clone());
        }

        Watch::new(rx, info)
    }

    /// Create a watch backed by a manual channel.
    ///
    /// Returns a sender and a `Watch<T>`. Send values through the sender
    /// to trigger the watch. Useful for non-filesystem triggers like health
    /// checks, polling, or external events.
    ///
    /// ```ignore
    /// let (tx, mut w) = ctx.watch_channel::<HealthStatus>();
    /// let w = w.label("health check");
    /// tokio::spawn(async move {
    ///     loop {
    ///         let status = poll_health().await;
    ///         tx.send(status).unwrap();
    ///         tokio::time::sleep(Duration::from_secs(5)).await;
    ///     }
    /// });
    /// loop {
    ///     let status = w.next().await;
    ///     // react...
    /// }
    /// ```
    pub fn watch_channel<T: Send + 'static>(&self) -> (mpsc::UnboundedSender<T>, Watch<T>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let info = Arc::new(std::sync::Mutex::new(WatchInfo {
            label: None,
            kind: WatchKind::Channel,
            trigger_count: 0,
            last_triggered: None,
        }));

        if let Ok(mut watches) = self.watches.lock() {
            watches.push(info.clone());
        }

        (tx, Watch::new(rx, info))
    }

    /// Set the working directory for file watches.
    ///
    /// By default, watches use the current working directory. Call this
    /// to override (e.g., to watch relative to the RUNME.rs file location).
    pub fn set_watch_dir(&mut self, dir: PathBuf) {
        self.watch_dir = dir;
    }

    /// Get the list of active watches for TUI visibility.
    pub fn watches(&self) -> Arc<std::sync::Mutex<Vec<Arc<std::sync::Mutex<WatchInfo>>>>> {
        self.watches.clone()
    }

    /// Access the output buffer (read the captured log entries).
    pub async fn output_lines(&self) -> Vec<LogEntry> {
        let buffer = self.output.lock().await;
        buffer.lines().iter().cloned().collect()
    }
}

/// Collects and looks up tasks.
pub struct Registry {
    tasks: Vec<&'static TaskDef>,
}

impl Registry {
    pub fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// Build a registry from all inventory-registered tasks.
    pub fn from_inventory() -> Self {
        let mut reg = Self::new();
        for task in inventory::iter::<TaskDef> {
            reg.tasks.push(task);
        }
        reg
    }

    pub fn register(&mut self, task: &'static TaskDef) {
        self.tasks.push(task);
    }

    pub fn get(&self, name: &str) -> Option<&'static TaskDef> {
        self.tasks.iter().find(|t| t.name == name).copied()
    }

    pub fn list(&self) -> &[&'static TaskDef] {
        &self.tasks
    }

    /// Look up a task by name, create a context, and call its function.
    pub async fn run(&self, name: &str) -> Result<(), TaskError> {
        match self.get(name) {
            Some(task) => {
                let ctx = TaskContext::new(task.name);
                (task.func)(&ctx).await
            }
            None => Err(TaskError::from_display(format!("unknown task: {}", name))),
        }
    }

    /// Run multiple tasks in parallel.
    pub async fn run_parallel(&self, names: &[&str]) -> Vec<Result<(), TaskError>> {
        let mut results = Vec::with_capacity(names.len());

        // Validate all tasks exist first
        let mut task_defs = Vec::new();
        for name in names {
            match self.get(name) {
                Some(def) => task_defs.push(def),
                None => {
                    results.push(Err(TaskError::from_display(format!(
                        "unknown task: {}",
                        name
                    ))));
                    return results;
                }
            }
        }

        let mut join_set = JoinSet::new();

        for task_def in &task_defs {
            let func = task_def.func;
            let name = task_def.name.to_string();
            join_set.spawn(async move {
                let ctx = TaskContext::new(&name);
                func(&ctx).await
            });
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(task_result) => results.push(task_result),
                Err(e) => results.push(Err(TaskError::from_display(e))),
            }
        }

        results
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dummy_task(
        ctx: &TaskContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
        Box::pin(async move {
            println!("Running dummy task: {}", ctx.name);
            Ok(())
        })
    }

    fn another_task(
        ctx: &TaskContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
        Box::pin(async move {
            println!("Running another task: {}", ctx.name);
            Ok(())
        })
    }

    static TEST_TASK_A: TaskDef = TaskDef {
        name: "alpha",
        description: Some("The alpha task"),
        group: "",
        depends_on: &[],
        func: dummy_task,
    };

    static TEST_TASK_B: TaskDef = TaskDef {
        name: "beta",
        description: None,
        group: "",
        depends_on: &["alpha"],
        func: another_task,
    };

    #[test]
    fn test_register_and_get() {
        let mut reg = Registry::new();
        reg.register(&TEST_TASK_A);
        reg.register(&TEST_TASK_B);

        let found = reg.get("alpha");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "alpha");
        assert_eq!(found.unwrap().description, Some("The alpha task"));

        let found_b = reg.get("beta");
        assert!(found_b.is_some());
        assert_eq!(found_b.unwrap().name, "beta");
    }

    #[test]
    fn test_get_missing() {
        let reg = Registry::new();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_list() {
        let mut reg = Registry::new();
        reg.register(&TEST_TASK_A);
        reg.register(&TEST_TASK_B);

        let all = reg.list();
        assert_eq!(all.len(), 2);
        let names: Vec<&str> = all.iter().map(|t| t.name).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[tokio::test]
    async fn test_run_calls_function() {
        let mut reg = Registry::new();
        reg.register(&TEST_TASK_A);
        reg.run("alpha").await.unwrap();
    }

    #[tokio::test]
    async fn test_run_unknown_task() {
        let reg = Registry::new();
        let result = reg.run("nonexistent").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "unknown task: nonexistent");
    }

    #[tokio::test]
    async fn test_exec_on_context() {
        let ctx = TaskContext::new("test");
        let result = ctx.exec("echo hello").await.unwrap();
        assert!(result.success());
        let stdout = result.output().stdout().await;
        assert_eq!(stdout, vec!["hello"]);
    }

    #[tokio::test]
    async fn test_spawn_on_context() {
        let ctx = TaskContext::new("test");
        let mut handle = ctx.spawn("sleep 60").await.unwrap();
        assert!(handle.is_running());
        handle.stop(Duration::from_secs(2)).await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_run_parallel() {
        fn task_a(
            ctx: &TaskContext,
        ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
            Box::pin(async move {
                println!("parallel task A: {}", ctx.name);
                Ok(())
            })
        }

        fn task_b(
            ctx: &TaskContext,
        ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
            Box::pin(async move {
                println!("parallel task B: {}", ctx.name);
                Ok(())
            })
        }

        static PARA: TaskDef = TaskDef {
            name: "para_a",
            description: Some("Parallel A"),
            group: "",
            depends_on: &[],
            func: task_a,
        };

        static PARB: TaskDef = TaskDef {
            name: "para_b",
            description: Some("Parallel B"),
            group: "",
            depends_on: &[],
            func: task_b,
        };

        let mut reg = Registry::new();
        reg.register(&PARA);
        reg.register(&PARB);

        let results = reg.run_parallel(&["para_a", "para_b"]).await;
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.is_ok());
        }
    }

    #[tokio::test]
    async fn test_run_parallel_missing_task() {
        let reg = Registry::new();
        let results = reg.run_parallel(&["nonexistent"]).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[tokio::test]
    async fn test_tui_wait_default_is_true() {
        let ctx = TaskContext::new("test");
        let flag = ctx.tui_wait_flag();
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_tui_wait_set_and_read() {
        let ctx = TaskContext::new("test");
        let flag = ctx.tui_wait_flag();

        ctx.tui_wait(false);
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));

        ctx.tui_wait(true);
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_tui_output_handle_is_shared() {
        let ctx = TaskContext::new("test");
        let handle1 = ctx.tui_output();
        let handle2 = ctx.tui_output();

        // Both handles point to the same underlying buffer
        handle1.write_stdout("from handle1").await;
        handle2.write_stderr("from handle2").await;

        let (stdout, stderr) = handle1.flush().await;
        assert_eq!(stdout, "from handle1\n");
        assert_eq!(stderr, "from handle2\n");
    }

    #[tokio::test]
    async fn test_task_output_returns_output() {
        let ctx = TaskContext::new("test");
        // task_output() should return an Output wrapping the task's buffer
        let output = ctx.task_output();
        // Initially empty
        let entries = output.entries().await;
        assert!(entries.is_empty());
    }
}
