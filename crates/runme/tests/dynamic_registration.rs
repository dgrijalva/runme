//! Design spike: dynamic task registration at runtime.
//!
//! # Problem
//!
//! Tasks are currently registered at compile time via `#[runme::task]` + `inventory`.
//! This works for tasks defined in source, but some use cases need runtime discovery:
//! - A "cargo" plugin that discovers available cargo subcommands and registers each as a task
//! - A "docker-compose" plugin that reads services from docker-compose.yml
//! - Any plugin system where capabilities are discovered at startup
//!
//! # Analysis of Options
//!
//! ## Option A: Extend InitContext with task collection
//!
//! Add a `Vec<DynamicTaskDef>` to InitContext. Init hooks push tasks, and the runner
//! feeds them into Registry after all init hooks complete.
//!
//! **Pros:**
//! - Minimal new API surface — extends an existing hook mechanism
//! - Natural ordering: init discovers, then tasks are available
//! - Single point of extension for RUNME.rs authors
//!
//! **Cons:**
//! - InitContext gains a second responsibility (configuration + registration)
//! - Dynamic tasks need a different type than TaskDef (owned strings, closures)
//! - Init functions are `fn(&mut InitContext)` (sync) — discovery might want async
//!
//! **Complexity:** Low-medium. Main work is DynamicTaskDef + plumbing through InitContext.
//!
//! ## Option B: Separate RegisterContext phase
//!
//! Add a new `#[runme::register]` hook that runs after init, receiving `&mut Registry`.
//! This is a distinct phase in the runner lifecycle.
//!
//! **Pros:**
//! - Clean separation of concerns (init = configure, register = add tasks)
//! - RegisterContext could provide &mut Registry directly
//! - Could be async from day one
//!
//! **Cons:**
//! - New macro, new hook type, new inventory collection — more API surface
//! - Two hooks per file instead of one for the common case
//! - The "register after init" ordering is implicit in the runner codegen, not enforced by types
//!
//! **Complexity:** Medium-high. Needs new macro support, new Def type, codegen changes.
//!
//! ## Option C: Make Registry accept owned strings (via DynamicTaskDef)
//!
//! Don't change InitContext or add new hooks. Instead, make Registry dual-mode: it holds
//! both `&'static TaskDef` (from inventory) and owned `DynamicTaskDef` entries. The caller
//! (runner codegen) can add dynamic tasks to Registry before dispatch.
//!
//! **Pros:**
//! - Registry is the single source of truth for all tasks
//! - No changes to InitContext or the hook system
//! - DynamicTaskDef can use owned Strings and closures naturally
//!
//! **Cons:**
//! - Where do dynamic tasks come from? Without a hook, there's no discovery mechanism.
//!   This option is necessary infrastructure but not sufficient by itself.
//! - Registry's get/list/resolve methods need to handle two storage types
//!
//! **Complexity:** Medium. The type work is straightforward but lookup unification needs care.
//!
//! # Recommendation: Option A + C combined
//!
//! **Option C (DynamicTaskDef + Registry support) is necessary infrastructure regardless.**
//! You need owned strings and closures to represent dynamic tasks. The question is just
//! how they get into the Registry.
//!
//! **Option A (InitContext collects tasks) is the simplest discovery hook.** It reuses an
//! existing mechanism that RUNME.rs authors already know about. The init function becomes:
//!
//! ```ignore
//! #[runme::init]
//! fn setup(ctx: &mut InitContext) {
//!     ctx.set_group_name("Cargo");
//!     // Discover cargo subcommands and register each as a task
//!     for cmd in discover_cargo_commands() {
//!         ctx.register_task(DynamicTaskDef::new(&cmd, move |ctx, _args| {
//!             Box::pin(async move {
//!                 ctx.exec(format!("cargo {}", cmd)).await?;
//!                 Ok(())
//!             })
//!         }));
//!     }
//! }
//! ```
//!
//! The runner codegen change is small: after running init hooks, drain collected tasks
//! from each InitContext and feed them into Registry before dispatch.
//!
//! **Why not Option B?** It adds a whole new hook type for something that fits naturally
//! into init. If init proves too limiting later (e.g., async discovery is needed), a
//! RegisterContext can be added then without breaking the init-based approach.
//!
//! # Key Design Decisions in This Prototype
//!
//! 1. **DynamicTaskFn uses `Box<dyn Fn>` instead of function pointers.** Function pointers
//!    can't capture state. Dynamic tasks inherently need captured state (e.g., which
//!    subcommand to run). The closure is `Fn` (not `FnOnce`) because tasks can be re-run.
//!
//! 2. **DynamicTaskDef uses owned `String` fields.** Static `&'static str` is impossible
//!    for runtime-generated names without `Box::leak` (which is a permanent allocation).
//!    Owned strings are the clean solution.
//!
//! 3. **Registry stores dynamic tasks separately** in a `Vec<DynamicTaskDef>`. The lookup
//!    methods check both collections. This avoids changing the static TaskDef type or
//!    introducing trait objects for the common path.
//!
//! 4. **`Box::leak` is avoided.** It works but permanently leaks memory proportional to
//!    the number of dynamic tasks. Since tasks are created once at startup and live for
//!    the process lifetime, the leak is bounded — but owned strings are cleaner and don't
//!    require unsafe-adjacent patterns.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use runme::error::TaskError;
#[allow(unused_imports)]
use runme::task::{ArgMetadataFn, Registry, TaskContext, TaskDef, UiHint};

// ---------------------------------------------------------------------------
// DynamicTaskFn — closure-based equivalent of TaskFn
// ---------------------------------------------------------------------------

/// A closure-based task function for dynamically registered tasks.
///
/// Unlike `TaskFn` (a function pointer), this can capture state. It's
/// `Send + Sync` so it can be shared across threads, and `Fn` (not `FnOnce`)
/// so the task can be invoked multiple times.
type DynamicTaskFn = Arc<
    dyn for<'a> Fn(&'a TaskContext, &[String]) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// DynamicTaskDef — owned equivalent of TaskDef
// ---------------------------------------------------------------------------

/// A task definition with owned data, for tasks created at runtime.
///
/// Mirrors `TaskDef` but uses `String` instead of `&'static str` and
/// `DynamicTaskFn` instead of `TaskFn`.
#[allow(dead_code)]
struct DynamicTaskDef {
    name: String,
    description: Option<String>,
    group: String,
    func: DynamicTaskFn,
    arg_metadata: ArgMetadataFn,
    ui_hint: Option<UiHint>,
}

impl DynamicTaskDef {
    /// Create a minimal dynamic task with just a name and function.
    fn new(
        name: impl Into<String>,
        func: impl for<'a> Fn(&'a TaskContext, &[String]) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: None,
            group: String::new(),
            func: Arc::new(func),
            arg_metadata: || None,
            ui_hint: None,
        }
    }

    /// Set a description.
    fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set the group.
    fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }
}

// ---------------------------------------------------------------------------
// DynamicRegistry — extends Registry concept with dynamic tasks
// ---------------------------------------------------------------------------

/// A registry that holds both static (inventory) and dynamic tasks.
///
/// In a real implementation, this would be integrated into Registry itself.
/// For this prototype, it wraps Registry and adds a dynamic task store.
struct DynamicRegistry {
    static_registry: Registry,
    dynamic_tasks: Vec<DynamicTaskDef>,
}

impl DynamicRegistry {
    fn new() -> Self {
        Self {
            static_registry: Registry::new(),
            dynamic_tasks: Vec::new(),
        }
    }

    fn from_inventory() -> Self {
        Self {
            static_registry: Registry::from_inventory(),
            dynamic_tasks: Vec::new(),
        }
    }

    /// Register a dynamic task.
    fn register_dynamic(&mut self, task: DynamicTaskDef) {
        self.dynamic_tasks.push(task);
    }

    /// Look up a task by name. Checks static tasks first, then dynamic.
    fn get_dynamic(&self, name: &str) -> Option<DynamicTaskRef<'_>> {
        // Static tasks take priority (they're the "real" definitions)
        if let Some(static_task) = self.static_registry.get(name) {
            return Some(DynamicTaskRef::Static(static_task));
        }
        // Then check dynamic tasks
        self.dynamic_tasks
            .iter()
            .find(|t| t.name == name)
            .map(DynamicTaskRef::Dynamic)
    }

    /// List all task names (static + dynamic).
    fn all_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .static_registry
            .list()
            .iter()
            .map(|t| t.name)
            .collect();
        for task in &self.dynamic_tasks {
            names.push(&task.name);
        }
        names
    }

    /// Run a task by name (static or dynamic).
    async fn run(&self, name: &str) -> Result<(), TaskError> {
        self.run_with_args(name, &[]).await
    }

    /// Run a task by name with arguments.
    async fn run_with_args(&self, name: &str, args: &[String]) -> Result<(), TaskError> {
        match self.get_dynamic(name) {
            Some(DynamicTaskRef::Static(task)) => {
                let ctx = TaskContext::new(task.name);
                (task.func)(&ctx, args).await
            }
            Some(DynamicTaskRef::Dynamic(task)) => {
                let ctx = TaskContext::new(&task.name);
                (task.func)(&ctx, args).await
            }
            None => Err(TaskError::from_display(format!("unknown task: {}", name))),
        }
    }
}

/// Reference to either a static or dynamic task, for unified lookup.
enum DynamicTaskRef<'a> {
    Static(&'static TaskDef),
    Dynamic(&'a DynamicTaskDef),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dynamic_task_runs_successfully() {
    let mut registry = DynamicRegistry::new();

    // Register a simple dynamic task
    registry.register_dynamic(DynamicTaskDef::new("hello", |_ctx, _args| {
        Box::pin(async { Ok(()) })
    }));

    // Run it
    let result = registry.run("hello").await;
    assert!(result.is_ok(), "dynamic task should succeed");
}

#[tokio::test]
async fn test_dynamic_task_with_captured_state() {
    let mut registry = DynamicRegistry::new();

    // Simulate discovering commands and creating tasks for each.
    // This is the "cargo subcommands" use case.
    let commands = vec!["build", "test", "clippy"];

    for cmd in &commands {
        let cmd_name = cmd.to_string();
        let task_name = format!("cargo_{}", cmd);

        registry.register_dynamic(
            DynamicTaskDef::new(task_name, move |_ctx, _args| {
                // The closure captures `cmd_name` — this is why we need
                // closures instead of function pointers.
                let cmd_name = cmd_name.clone();
                Box::pin(async move {
                    // In real code: ctx.exec(format!("cargo {}", cmd_name)).await?;
                    assert!(!cmd_name.is_empty(), "captured command name should be present");
                    Ok(())
                })
            })
            .with_description(format!("Run cargo {}", cmd))
            .with_group("cargo".to_string()),
        );
    }

    // Verify all three tasks are registered and runnable
    assert_eq!(registry.dynamic_tasks.len(), 3);

    registry.run("cargo_build").await.unwrap();
    registry.run("cargo_test").await.unwrap();
    registry.run("cargo_clippy").await.unwrap();
}

#[tokio::test]
async fn test_dynamic_task_can_return_error() {
    let mut registry = DynamicRegistry::new();

    registry.register_dynamic(DynamicTaskDef::new("failing_task", |_ctx, _args| {
        Box::pin(async { Err(TaskError::from_display("something went wrong")) })
    }));

    let result = registry.run("failing_task").await;
    assert!(result.is_err(), "task should fail");
    assert!(
        result.unwrap_err().to_string().contains("something went wrong"),
        "error message should propagate"
    );
}

#[tokio::test]
async fn test_dynamic_task_receives_arguments() {
    let mut registry = DynamicRegistry::new();

    registry.register_dynamic(DynamicTaskDef::new("echo_args", |_ctx, args| {
        let args = args.to_vec();
        Box::pin(async move {
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "hello");
            assert_eq!(args[1], "world");
            Ok(())
        })
    }));

    let args: Vec<String> = vec!["hello".into(), "world".into()];
    registry.run_with_args("echo_args", &args).await.unwrap();
}

#[tokio::test]
async fn test_dynamic_and_static_tasks_coexist() {
    let mut registry = DynamicRegistry::from_inventory();

    // Add a dynamic task alongside whatever static tasks exist
    registry.register_dynamic(DynamicTaskDef::new("dynamic_one", |_ctx, _args| {
        Box::pin(async { Ok(()) })
    }));

    // The dynamic task should be findable
    assert!(
        registry.get_dynamic("dynamic_one").is_some(),
        "dynamic task should be found"
    );

    // Static tasks from inventory should still be present
    let all_names = registry.all_names();
    assert!(
        all_names.contains(&"dynamic_one"),
        "dynamic task should appear in listing"
    );

    // Run the dynamic task
    registry.run("dynamic_one").await.unwrap();
}

#[tokio::test]
async fn test_unknown_task_returns_error() {
    let registry = DynamicRegistry::new();
    let result = registry.run("nonexistent").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("unknown task"));
}

#[tokio::test]
async fn test_dynamic_task_with_description_and_group() {
    let mut registry = DynamicRegistry::new();

    registry.register_dynamic(
        DynamicTaskDef::new("my_task", |_ctx, _args| Box::pin(async { Ok(()) }))
            .with_description("A dynamically created task")
            .with_group("plugins"),
    );

    let task = match registry.get_dynamic("my_task") {
        Some(DynamicTaskRef::Dynamic(t)) => t,
        _ => panic!("should find dynamic task"),
    };

    assert_eq!(task.name, "my_task");
    assert_eq!(task.description.as_deref(), Some("A dynamically created task"));
    assert_eq!(task.group, "plugins");
}

#[tokio::test]
async fn test_dynamic_task_rerunnable() {
    // Tasks use Fn (not FnOnce), so they should be runnable multiple times.
    let mut registry = DynamicRegistry::new();

    registry.register_dynamic(DynamicTaskDef::new("rerun_me", |_ctx, _args| {
        Box::pin(async { Ok(()) })
    }));

    // Run it three times
    registry.run("rerun_me").await.unwrap();
    registry.run("rerun_me").await.unwrap();
    registry.run("rerun_me").await.unwrap();
}

/// Simulates the InitContext extension pattern from Option A.
///
/// In the real implementation, InitContext would gain a `register_task` method
/// and an internal Vec<DynamicTaskDef>. After init hooks complete, the runner
/// drains those tasks into the Registry.
#[tokio::test]
async fn test_init_context_collection_pattern() {
    // Simulate what InitContext would look like with task collection
    #[allow(dead_code)]
    struct InitContextWithTasks {
        group_name: String,
        collected_tasks: Vec<DynamicTaskDef>,
    }

    impl InitContextWithTasks {
        fn new(group: &str) -> Self {
            Self {
                group_name: group.to_string(),
                collected_tasks: Vec::new(),
            }
        }

        fn register_task(&mut self, task: DynamicTaskDef) {
            self.collected_tasks.push(task);
        }
    }

    // --- Simulate the runner lifecycle ---

    // 1. Init phase: init hook discovers capabilities
    let mut init_ctx = InitContextWithTasks::new("docker");

    // Simulate discovering docker-compose services
    let services = vec!["web", "db", "redis"];
    for svc in &services {
        let svc_name = svc.to_string();
        let task_name = format!("up_{}", svc);
        init_ctx.register_task(
            DynamicTaskDef::new(task_name, move |_ctx, _args| {
                let svc_name = svc_name.clone();
                Box::pin(async move {
                    // In real code: ctx.exec(format!("docker compose up {}", svc_name)).await?;
                    let _ = svc_name;
                    Ok(())
                })
            })
            .with_group("docker"),
        );
    }

    // 2. Post-init: runner drains tasks into registry
    let mut registry = DynamicRegistry::from_inventory();
    for task in init_ctx.collected_tasks {
        registry.register_dynamic(task);
    }

    // 3. Dispatch phase: tasks are available
    registry.run("up_web").await.unwrap();
    registry.run("up_db").await.unwrap();
    registry.run("up_redis").await.unwrap();

    // Verify all services are registered
    let names = registry.all_names();
    assert!(names.contains(&"up_web"));
    assert!(names.contains(&"up_db"));
    assert!(names.contains(&"up_redis"));
}
