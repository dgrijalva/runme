use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::cmd::Cmd;
use crate::error::TaskError;
use crate::process::{self, ExecOutput, LogLine, OutputBuffer, ProcessError, ProcessHandle};

/// The type of async task functions.
///
/// Task functions are `async fn(&TaskContext) -> Result<(), TaskError>` — this
/// type alias represents that as a function pointer returning a boxed future.
pub type TaskFn = fn(&TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>>;

/// Task metadata — what the macro extracts and registers.
///
/// Uses `&'static str` for name/description so that instances can be
/// constructed in static context and satisfy `Send + Sync + 'static`
/// (required by `inventory`).
pub struct TaskDef {
    pub name: &'static str,
    pub description: Option<&'static str>,
    pub watch: Option<&'static str>,
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
    output: Mutex<OutputBuffer>,
}

impl TaskContext {
    /// Create a new TaskContext for the named task.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            output: Mutex::new(OutputBuffer::new(10_000)),
        }
    }

    /// Create a new TaskContext with a custom buffer capacity.
    pub fn with_capacity(name: impl Into<String>, capacity: usize) -> Self {
        Self {
            name: name.into(),
            output: Mutex::new(OutputBuffer::new(capacity)),
        }
    }

    /// Run a command and wait for it to complete. Captures output.
    ///
    /// Accepts a `Cmd`, `&str`, or `String`. Strings are treated as shell commands.
    pub async fn exec(&self, command: impl Into<Cmd>) -> Result<ExecOutput, ProcessError> {
        let mut buffer = self.output.lock().await;
        process::exec(command, &self.name, &mut buffer).await
    }

    /// Spawn a long-running command. Returns a handle for monitoring/control.
    ///
    /// Accepts a `Cmd`, `&str`, or `String`. Strings are treated as shell commands.
    pub async fn spawn(&self, command: impl Into<Cmd>) -> Result<ProcessHandle, ProcessError> {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(10_000)));
        process::spawn(command, &self.name, buffer).await
    }

    /// Access the output buffer (read the captured lines).
    pub async fn output_lines(&self) -> Vec<LogLine> {
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

    fn dummy_task(ctx: &TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
        Box::pin(async move {
            println!("Running dummy task: {}", ctx.name);
            Ok(())
        })
    }

    fn another_task(ctx: &TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
        Box::pin(async move {
            println!("Running another task: {}", ctx.name);
            Ok(())
        })
    }

    static TEST_TASK_A: TaskDef = TaskDef {
        name: "alpha",
        description: Some("The alpha task"),
        watch: None,
        depends_on: &[],
        func: dummy_task,
    };

    static TEST_TASK_B: TaskDef = TaskDef {
        name: "beta",
        description: None,
        watch: Some("src/**/*.rs"),
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
        assert_eq!(found_b.unwrap().watch, Some("src/**/*.rs"));
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
        let output = ctx.exec("echo hello").await.unwrap();
        assert_eq!(output.stdout, "hello\n");
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
        fn task_a(ctx: &TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
            Box::pin(async move {
                println!("parallel task A: {}", ctx.name);
                Ok(())
            })
        }

        fn task_b(ctx: &TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
            Box::pin(async move {
                println!("parallel task B: {}", ctx.name);
                Ok(())
            })
        }

        static PARA: TaskDef = TaskDef {
            name: "para_a",
            description: Some("Parallel A"),
            watch: None,
            depends_on: &[],
            func: task_a,
        };

        static PARB: TaskDef = TaskDef {
            name: "para_b",
            description: Some("Parallel B"),
            watch: None,
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
}
