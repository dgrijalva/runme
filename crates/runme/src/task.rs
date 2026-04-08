use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use globset::GlobBuilder;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;

use crate::cmd::Cmd;
use crate::error::{TaskError, TaskResult};
use crate::log::LogEntry;
use crate::log::buffer::OutputBuffer;
use crate::process::{self, Output, ProcessError, ProcessHandle, ProcessResult};
use crate::tui::output::{TuiOutput, TuiOutputHandle};
use crate::watch::{self, Watch, WatchInfo, WatchKind};

/// The type of static async task functions (from `#[runme::task]` macro).
///
/// A function pointer returning a boxed future. Used in `inventory::submit!`
/// which requires const-constructible values.
pub type TaskFn = for<'a> fn(
    &'a TaskContext,
    &[String],
) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>>;

/// The type of dynamic async task functions (from runtime registration).
///
/// An `Arc<dyn Fn>` that can capture state — needed for dynamic tasks like
/// "run cargo {subcommand}" where the subcommand is discovered at init time.
pub type DynamicTaskFn = Arc<
    dyn for<'a> Fn(
            &'a TaskContext,
            &[String],
        ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>>
        + Send
        + Sync,
>;

/// How a task function is stored — either a compile-time function pointer
/// or a runtime closure.
#[derive(Clone)]
pub enum TaskFnKind {
    /// From `#[runme::task]` — const-constructible for `inventory::submit!`.
    Static(TaskFn),
    /// From `InitContext::register_task()` — can capture state.
    Dynamic(DynamicTaskFn),
}

impl TaskFnKind {
    /// Call the task function, regardless of variant.
    pub fn call<'a>(
        &self,
        ctx: &'a TaskContext,
        args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        match self {
            TaskFnKind::Static(f) => f(ctx, args),
            TaskFnKind::Dynamic(f) => f(ctx, args),
        }
    }
}

/// Function that returns clap metadata for a task's arguments, if any.
///
/// Returns `None` for zero-arg tasks. The `clap::Command` describes the
/// argument schema so that `--help` and validation work.
pub type ArgMetadataFn = fn() -> Option<clap::Command>;

/// Task metadata — what the macro extracts and registers.
///
/// Uses `&'static str` for name/description so that instances can be
/// constructed in static context and satisfy `Send + Sync + 'static`
/// (required by `inventory`).
/// UI mode hint — tasks can declare a preferred execution mode.
///
/// When set on a `TaskDef`, the CLI dispatch will use this mode instead of
/// the user's `--ui` flag. Useful for utility tasks (like `list`) that
/// should always run in CLI mode regardless of the default.
#[derive(Clone, Copy, Debug)]
pub enum UiHint {
    /// Interactive TUI with log viewer
    Tui,
    /// Direct CLI execution with stdio output
    Cli,
}

pub struct TaskDef {
    pub name: &'static str,
    pub description: Option<&'static str>,
    pub group: &'static str,
    pub func: TaskFnKind,
    pub arg_metadata: ArgMetadataFn,
    /// Optional UI mode override. When set, the CLI dispatch uses this
    /// instead of the user's `--ui` flag.
    pub ui_hint: Option<UiHint>,
}

// Safety: TaskDef fields are Send + Sync: &'static str, function pointers,
// and TaskFnKind (fn pointer or Arc<dyn Fn + Send + Sync>).
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
    /// Shared registry for cross-file task invocation via `ctx.run()`.
    /// Injected by the runtime when tasks are launched through `Registry::run_with_registry()`.
    /// `None` when running outside the full runtime (e.g., standalone tests).
    registry: Option<Arc<Registry>>,
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
            registry: None,
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
            registry: None,
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

    /// Print raw, undecorated text to the task output.
    ///
    /// Works in all modes:
    /// - **TUI**: appears as a raw entry in the log viewer
    /// - **CLI**: forwarded to stdout by the output subscriber
    /// - **ctx.run()**: flows through the calling task's log engine
    ///
    /// Use this instead of `println!()` for task output that should be
    /// visible regardless of UI mode.
    pub async fn println(&self, text: impl std::fmt::Display) {
        let text = text.to_string();
        let entry = LogEntry::raw(&text, &self.name);
        // Push to the tracing buffer if available (TUI mode), otherwise exec buffer
        let buffer = self
            .tracing_output
            .as_ref()
            .unwrap_or(&self.output);
        buffer.lock().await.push(entry);
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

    /// Get the tracked process group IDs (for testing/inspection).
    pub async fn spawned_pgids(&self) -> tokio::sync::MutexGuard<'_, Vec<i32>> {
        self.spawned_pgids.lock().await
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

    /// Start observing a task. Returns a `TaskGuard` that emits a tracing
    /// event when dropped.
    pub fn start_task(&self, name: &str) -> TaskGuard {
        TaskGuard::new(name)
    }

    /// Begin a step within the current task. Returns a `StepGuard` that emits
    /// a tracing event when dropped, recording success or failure.
    pub fn begin_step(&self, name: &str) -> StepGuard {
        StepGuard::new(name)
    }

    /// Set the shared registry for cross-file task invocation.
    ///
    /// Called by the runtime when launching tasks through `Registry::run_with_registry()`.
    /// Once set, `ctx.run()` and `ctx.tasks()` become available.
    pub fn set_registry(&mut self, registry: Arc<Registry>) {
        self.registry = Some(registry);
    }

    /// Invoke a task by name with string arguments.
    ///
    /// Resolves the task through the registry (supporting short names,
    /// `group:task` qualified names, and `:builtin` aliases) and runs it.
    /// The invoked task receives its own `TaskContext` with the same
    /// registry, enabling transitive cross-file invocation.
    ///
    /// Returns an error if no registry is available (e.g., standalone test
    /// context) or if the task cannot be resolved.
    ///
    /// ```ignore
    /// ctx.run("test", &["--verbose"]).await?;
    /// ctx.run("services/auth:deploy", &["--env", "staging"]).await?;
    /// ctx.run(":list", &[]).await?;
    /// ```
    pub async fn run(&self, name: &str, args: &[&str]) -> TaskResult {
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| TaskError::from_display("no registry available"))?;
        let string_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        registry.run_with_registry(name, &string_args, registry).await
    }

    /// Query the task registry for discovery and listing.
    ///
    /// Returns `None` if no registry is available (standalone test context).
    ///
    /// ```ignore
    /// if let Some(query) = ctx.tasks() {
    ///     for task in query.all() {
    ///         println!("{}: {}", task.qualified_name, task.description.unwrap_or(""));
    ///     }
    ///     for task in query.matching("services/*:test") {
    ///         ctx.run(&task.qualified_name, &[]).await?;
    ///     }
    /// }
    /// ```
    pub fn tasks(&self) -> Option<TaskQuery> {
        self.registry.as_ref().map(|r| TaskQuery {
            registry: r.clone(),
        })
    }

    /// Access the output buffer (read the captured log entries).
    pub async fn output_lines(&self) -> Vec<LogEntry> {
        let buffer = self.output.lock().await;
        buffer.lines().iter().cloned().collect()
    }
}

/// Lightweight task descriptor for discovery and listing.
///
/// Unlike `TaskDef` (which carries a function pointer and is `'static`),
/// `TaskInfo` is an owned value suitable for serialization, display, or
/// passing across API boundaries.
pub struct TaskInfo {
    pub name: &'static str,
    pub group: &'static str,
    pub description: Option<&'static str>,
    /// Fully qualified name: "group:name" for grouped tasks, just "name" for root.
    pub qualified_name: String,
}

impl TaskInfo {
    /// Build a `TaskInfo` from a `TaskDef`.
    pub fn from_def(def: &'static TaskDef) -> Self {
        let qualified_name = if def.group.is_empty() {
            def.name.to_string()
        } else {
            format!("{}:{}", def.group, def.name)
        };
        Self {
            name: def.name,
            group: def.group,
            description: def.description,
            qualified_name,
        }
    }
}

/// Query handle for discovering tasks in the registry.
///
/// Obtained via `ctx.tasks()`. Supports listing all tasks or matching
/// by glob pattern against qualified names (`"group:name"`).
pub struct TaskQuery {
    registry: Arc<Registry>,
}

impl TaskQuery {
    /// Return info for all registered tasks.
    pub fn all(&self) -> Vec<TaskInfo> {
        self.registry.list().iter().map(|def| TaskInfo::from_def(def)).collect()
    }

    /// Return tasks whose qualified name matches a glob pattern.
    ///
    /// The pattern is matched against the fully qualified name: `"group:name"`
    /// for grouped tasks, or just `"name"` for root tasks.
    ///
    /// Examples: `"*:test"`, `"services/*:deploy"`, `"build"`.
    pub fn matching(&self, pattern: &str) -> Vec<TaskInfo> {
        let glob = match GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
        {
            Ok(g) => g.compile_matcher(),
            Err(_) => return Vec::new(),
        };

        self.registry
            .list()
            .iter()
            .filter_map(|def| {
                let info = TaskInfo::from_def(def);
                if glob.is_match(&info.qualified_name) {
                    Some(info)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// RAII guard for task-level observation.
///
/// Created via `TaskContext::start_task()`. Emits a tracing event when
/// dropped, recording task entry and exit for observability tooling.
pub struct TaskGuard {
    name: String,
}

impl TaskGuard {
    fn new(name: &str) -> Self {
        tracing::info!(task = name, event = "task_start", "task started");
        Self {
            name: name.to_string(),
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        tracing::info!(task = %self.name, event = "task_end", "task ended");
    }
}

/// RAII guard for step-level observation within a task.
///
/// Created via `TaskContext::begin_step()`. Call `fail()` to mark the step
/// as failed before it is dropped. On drop, emits a tracing event recording
/// success or failure.
pub struct StepGuard {
    name: String,
    failed: bool,
    failure_reason: Option<String>,
}

impl StepGuard {
    fn new(name: &str) -> Self {
        tracing::info!(step = name, event = "step_start", "step started");
        Self {
            name: name.to_string(),
            failed: false,
            failure_reason: None,
        }
    }

    /// Mark the step as failed with a reason.
    pub fn fail(&mut self, reason: &str) {
        self.failed = true;
        self.failure_reason = Some(reason.to_string());
    }
}

impl Drop for StepGuard {
    fn drop(&mut self) {
        if self.failed {
            let reason = self.failure_reason.as_deref().unwrap_or("unknown");
            tracing::error!(
                step = %self.name,
                event = "step_end",
                success = false,
                reason = reason,
                "step failed"
            );
        } else {
            tracing::info!(
                step = %self.name,
                event = "step_end",
                success = true,
                "step completed"
            );
        }
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
        self.run_with_args(name, &[]).await
    }

    /// Look up a task by name, create a context, and call its function with arguments.
    pub async fn run_with_args(&self, name: &str, args: &[String]) -> Result<(), TaskError> {
        match self.get(name) {
            Some(task) => {
                let ctx = TaskContext::new(task.name);
                task.func.call(&ctx, args).await
            }
            None => Err(TaskError::from_display(format!("unknown task: {}", name))),
        }
    }

    /// Resolve and run a task, injecting the registry into the TaskContext.
    ///
    /// This is the primary entry point for cross-file invocation. The created
    /// `TaskContext` receives the registry so that the invoked task can itself
    /// call `ctx.run()` for transitive invocation.
    pub async fn run_with_registry(
        &self,
        name: &str,
        args: &[String],
        registry: &Arc<Registry>,
    ) -> Result<(), TaskError> {
        let task = self.resolve(name)?;
        let mut ctx = TaskContext::new(task.name);
        ctx.set_registry(registry.clone());
        task.func.call(&ctx, args).await
    }

    /// Resolve a task name to a `TaskDef` using 3-tier resolution.
    ///
    /// Resolution rules:
    /// 1. **`:` prefix** — stripped and resolved as `builtin:name`
    ///    (e.g., `:list` finds the `list` task in group `"builtin"`)
    /// 2. **`group:name` qualified** — exact match on group key and task name
    /// 3. **Short name** — match by task name alone:
    ///    - If exactly one task matches, return it
    ///    - If multiple match but one is in the root group (`""`), root wins
    ///    - Otherwise, return an error listing the qualified alternatives
    pub fn resolve(&self, name: &str) -> Result<&'static TaskDef, TaskError> {
        // Handle `:` prefix → builtin: group
        if let Some(short) = name.strip_prefix(':') {
            return self
                .tasks
                .iter()
                .find(|t| t.name == short && t.group == "builtin")
                .copied()
                .ok_or_else(|| {
                    TaskError::from_display(format!("unknown built-in task: {}", short))
                });
        }

        // Handle group:task explicit lookup
        if let Some((group, task_name)) = name.split_once(':') {
            return self
                .tasks
                .iter()
                .find(|t| t.name == task_name && t.group == group)
                .copied()
                .ok_or_else(|| TaskError::from_display(format!("unknown task: {}", name)));
        }

        // Short name lookup with root-wins disambiguation
        let matches: Vec<&'static TaskDef> = self
            .tasks
            .iter()
            .filter(|t| t.name == name)
            .copied()
            .collect();

        match matches.len() {
            0 => Err(TaskError::from_display(format!("unknown task: {}", name))),
            1 => Ok(matches[0]),
            _ => {
                // Root group ("") wins short names
                if let Some(root_task) = matches.iter().find(|t| t.group.is_empty()) {
                    Ok(root_task)
                } else {
                    let qualified: Vec<String> = matches
                        .iter()
                        .map(|t| {
                            if t.group.is_empty() {
                                t.name.to_string()
                            } else {
                                format!("{}:{}", t.group, t.name)
                            }
                        })
                        .collect();
                    Err(TaskError::from_display(format!(
                        "ambiguous task '{}', use: {}",
                        name,
                        qualified.join(", ")
                    )))
                }
            }
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
            let func = task_def.func.clone();
            let name = task_def.name.to_string();
            join_set.spawn(async move {
                let ctx = TaskContext::new(&name);
                func.call(&ctx, &[]).await
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

    fn no_arg_metadata() -> Option<clap::Command> {
        None
    }

    fn dummy_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        Box::pin(async move {
            println!("Running dummy task: {}", ctx.name);
            Ok(())
        })
    }

    fn another_task<'a>(
        ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        Box::pin(async move {
            println!("Running another task: {}", ctx.name);
            Ok(())
        })
    }

    static TEST_TASK_A: TaskDef = TaskDef {
        name: "alpha",
        description: Some("The alpha task"),
        group: "",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_B: TaskDef = TaskDef {
        name: "beta",
        description: None,
        group: "",
        func: TaskFnKind::Static(another_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
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
        fn task_a<'a>(
            ctx: &'a TaskContext,
            _args: &[String],
        ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
            Box::pin(async move {
                println!("parallel task A: {}", ctx.name);
                Ok(())
            })
        }

        fn task_b<'a>(
            ctx: &'a TaskContext,
            _args: &[String],
        ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
            Box::pin(async move {
                println!("parallel task B: {}", ctx.name);
                Ok(())
            })
        }

        static PARA: TaskDef = TaskDef {
            name: "para_a",
            description: Some("Parallel A"),
            group: "",
            func: TaskFnKind::Static(task_a),
            arg_metadata: no_arg_metadata,
            ui_hint: None,
        };

        static PARB: TaskDef = TaskDef {
            name: "para_b",
            description: Some("Parallel B"),
            group: "",
            func: TaskFnKind::Static(task_b),
            arg_metadata: no_arg_metadata,
            ui_hint: None,
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

    // --- resolve() tests ---

    // Additional static task defs for resolve tests (various groups)
    static RESOLVE_ROOT_BUILD: TaskDef = TaskDef {
        name: "build",
        description: Some("Root build"),
        group: "",

        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static RESOLVE_SERVICES_BUILD: TaskDef = TaskDef {
        name: "build",
        description: Some("Services build"),
        group: "services",

        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static RESOLVE_AUTH_DEPLOY: TaskDef = TaskDef {
        name: "deploy",
        description: Some("Auth deploy"),
        group: "services/auth",

        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static RESOLVE_WEB_DEPLOY: TaskDef = TaskDef {
        name: "deploy",
        description: Some("Web deploy"),
        group: "web",

        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static RESOLVE_BUILTIN_LIST: TaskDef = TaskDef {
        name: "list",
        description: Some("List tasks"),
        group: "builtin",

        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    fn resolve_registry() -> Registry {
        let mut reg = Registry::new();
        reg.register(&RESOLVE_ROOT_BUILD);
        reg.register(&RESOLVE_SERVICES_BUILD);
        reg.register(&RESOLVE_AUTH_DEPLOY);
        reg.register(&RESOLVE_WEB_DEPLOY);
        reg.register(&RESOLVE_BUILTIN_LIST);
        reg
    }

    #[test]
    fn test_resolve_exact_short_name() {
        let reg = resolve_registry();
        // "deploy" exists in two non-root groups, but "alpha" is unique in root
        let mut reg2 = Registry::new();
        reg2.register(&TEST_TASK_A); // "alpha" in root group
        let task = reg2.resolve("alpha").unwrap();
        assert_eq!(task.name, "alpha");

        // Single match in a non-root group also resolves
        let task = reg.resolve("list").unwrap();
        assert_eq!(task.name, "list");
        assert_eq!(task.group, "builtin");
    }

    #[test]
    fn test_resolve_group_task_qualified() {
        let reg = resolve_registry();
        let task = reg.resolve("services/auth:deploy").unwrap();
        assert_eq!(task.name, "deploy");
        assert_eq!(task.group, "services/auth");

        let task = reg.resolve("web:deploy").unwrap();
        assert_eq!(task.name, "deploy");
        assert_eq!(task.group, "web");

        let task = reg.resolve("services:build").unwrap();
        assert_eq!(task.name, "build");
        assert_eq!(task.group, "services");
    }

    #[test]
    fn test_resolve_ambiguous_root_wins() {
        let reg = resolve_registry();
        // "build" exists in root ("") and in "services". Root wins.
        let task = reg.resolve("build").unwrap();
        assert_eq!(task.name, "build");
        assert_eq!(task.group, "");
    }

    #[test]
    fn test_resolve_ambiguous_no_root_errors() {
        let reg = resolve_registry();
        // "deploy" exists in "services/auth" and "web", neither is root.
        let result = reg.resolve("deploy");
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("ambiguous task 'deploy'"));
        assert!(msg.contains("services/auth:deploy"));
        assert!(msg.contains("web:deploy"));
    }

    #[test]
    fn test_resolve_colon_prefix_builtin() {
        let reg = resolve_registry();
        let task = reg.resolve(":list").unwrap();
        assert_eq!(task.name, "list");
        assert_eq!(task.group, "builtin");
    }

    #[test]
    fn test_resolve_colon_prefix_unknown() {
        let reg = resolve_registry();
        let result = reg.resolve(":nonexistent");
        assert!(result.is_err());
        assert_eq!(
            result.err().unwrap().to_string(),
            "unknown built-in task: nonexistent"
        );
    }

    #[test]
    fn test_resolve_unknown_task() {
        let reg = resolve_registry();
        let result = reg.resolve("nonexistent");
        assert!(result.is_err());
        assert_eq!(
            result.err().unwrap().to_string(),
            "unknown task: nonexistent"
        );
    }

    #[test]
    fn test_resolve_unknown_qualified() {
        let reg = resolve_registry();
        let result = reg.resolve("nope:build");
        assert!(result.is_err());
        assert_eq!(
            result.err().unwrap().to_string(),
            "unknown task: nope:build"
        );
    }

    // --- TaskQuery tests ---

    #[test]
    fn test_task_query_all() {
        let reg = Arc::new(resolve_registry());
        let query = TaskQuery {
            registry: reg.clone(),
        };
        let all = query.all();
        assert_eq!(all.len(), 5);
        let names: Vec<&str> = all.iter().map(|t| t.name).collect();
        assert!(names.contains(&"build"));
        assert!(names.contains(&"deploy"));
        assert!(names.contains(&"list"));
    }

    #[test]
    fn test_task_query_matching() {
        let reg = Arc::new(resolve_registry());
        let query = TaskQuery {
            registry: reg.clone(),
        };

        // Match all deploy tasks
        let deploys = query.matching("*:deploy");
        assert_eq!(deploys.len(), 2);
        let groups: Vec<&str> = deploys.iter().map(|t| t.group).collect();
        assert!(groups.contains(&"services/auth"));
        assert!(groups.contains(&"web"));

        // Match root tasks (no colon in qualified name)
        let root = query.matching("build");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].group, "");

        // Match everything
        let all = query.matching("*");
        // Root "build" has qualified name "build" (no colon), matches "*"
        // Others have "group:name", which also matches "*"
        assert!(!all.is_empty());
    }

    #[test]
    fn test_task_query_matching_no_results() {
        let reg = Arc::new(resolve_registry());
        let query = TaskQuery {
            registry: reg.clone(),
        };
        let results = query.matching("nonexistent:*");
        assert!(results.is_empty());
    }

    // --- ctx.run() and ctx.tasks() tests ---

    #[tokio::test]
    async fn test_ctx_run_with_registry() {
        let mut reg = Registry::new();
        reg.register(&TEST_TASK_A);
        let reg = Arc::new(reg);

        let mut ctx = TaskContext::new("caller");
        ctx.set_registry(reg.clone());

        // Should be able to invoke "alpha" through ctx.run()
        let result = ctx.run("alpha", &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_ctx_run_without_registry() {
        let ctx = TaskContext::new("caller");
        let result = ctx.run("alpha", &[]).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "no registry available");
    }

    #[test]
    fn test_ctx_tasks_with_registry() {
        let mut reg = Registry::new();
        reg.register(&TEST_TASK_A);
        reg.register(&TEST_TASK_B);
        let reg = Arc::new(reg);

        let mut ctx = TaskContext::new("caller");
        ctx.set_registry(reg.clone());

        let query = ctx.tasks();
        assert!(query.is_some());
        let all = query.unwrap().all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_ctx_tasks_without_registry() {
        let ctx = TaskContext::new("caller");
        assert!(ctx.tasks().is_none());
    }

    // --- run_with_registry test ---

    #[tokio::test]
    async fn test_run_with_registry() {
        let mut reg = Registry::new();
        reg.register(&TEST_TASK_A);
        let reg = Arc::new(reg);

        let result = reg.run_with_registry("alpha", &[], &reg).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_with_registry_resolve_fails() {
        let reg = Arc::new(Registry::new());
        let result = reg.run_with_registry("nonexistent", &[], &reg).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "unknown task: nonexistent"
        );
    }
}
