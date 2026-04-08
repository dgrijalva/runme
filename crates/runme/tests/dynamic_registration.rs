//! Integration tests for dynamic task registration via InitContext.
//!
//! Dynamic tasks are registered at init time using `InitContext::register_task()`.
//! They use `TaskFnKind::Dynamic(Arc<dyn Fn>)` under the hood, which allows
//! closures that capture state — unlike static tasks which use function pointers.
//!
//! The strings (name, description, group) are leaked to `&'static str` by
//! `register_task()`, which is fine since tasks live for the process lifetime.

use runme::error::TaskError;
use runme::init::InitContext;
use runme::task::Registry;

// ---------------------------------------------------------------------------
// Basic registration and execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dynamic_task_runs_successfully() {
    let mut ctx = InitContext::new("test");
    ctx.register_task("hello", None, |_ctx, _args| {
        Box::pin(async { Ok(()) })
    });

    let mut reg = Registry::new();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    let result = reg.run("hello").await;
    assert!(result.is_ok(), "dynamic task should succeed");
}

#[tokio::test]
async fn test_dynamic_task_with_captured_state() {
    let mut ctx = InitContext::new("cargo");

    // Simulate discovering commands and creating tasks for each
    let commands = vec!["build", "test", "clippy"];
    for cmd in &commands {
        let cmd_name = cmd.to_string();
        ctx.register_task(
            &format!("cargo_{}", cmd),
            Some(&format!("Run cargo {}", cmd)),
            move |_ctx, _args| {
                let cmd_name = cmd_name.clone();
                Box::pin(async move {
                    assert!(!cmd_name.is_empty(), "captured command name should be present");
                    Ok(())
                })
            },
        );
    }

    let mut reg = Registry::new();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    assert_eq!(reg.list().len(), 3);
    reg.run("cargo_build").await.unwrap();
    reg.run("cargo_test").await.unwrap();
    reg.run("cargo_clippy").await.unwrap();
}

#[tokio::test]
async fn test_dynamic_task_can_return_error() {
    let mut ctx = InitContext::new("");
    ctx.register_task("failing", None, |_ctx, _args| {
        Box::pin(async { Err(TaskError::from_display("something went wrong")) })
    });

    let mut reg = Registry::new();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    let result = reg.run("failing").await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("something went wrong"));
}

#[tokio::test]
async fn test_dynamic_task_receives_arguments() {
    let mut ctx = InitContext::new("");
    ctx.register_task("echo_args", None, |_ctx, args| {
        let args = args.to_vec();
        Box::pin(async move {
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "hello");
            assert_eq!(args[1], "world");
            Ok(())
        })
    });

    let mut reg = Registry::new();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    let args: Vec<String> = vec!["hello".into(), "world".into()];
    reg.run_with_args("echo_args", &args).await.unwrap();
}

// ---------------------------------------------------------------------------
// Coexistence with static tasks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dynamic_and_static_tasks_coexist() {
    let mut ctx = InitContext::new("");
    ctx.register_task("dynamic_one", None, |_ctx, _args| {
        Box::pin(async { Ok(()) })
    });

    // Start from inventory (picks up static tasks from this crate)
    let mut reg = Registry::from_inventory();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    assert!(reg.get("dynamic_one").is_some());
    reg.run("dynamic_one").await.unwrap();
}

// ---------------------------------------------------------------------------
// Metadata: description, group
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dynamic_task_metadata() {
    let mut ctx = InitContext::new("plugins");
    ctx.register_task(
        "my_task",
        Some("A dynamically created task"),
        |_ctx, _args| Box::pin(async { Ok(()) }),
    );

    let mut reg = Registry::new();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    let task = reg.get("my_task").unwrap();
    assert_eq!(task.name, "my_task");
    assert_eq!(task.description, Some("A dynamically created task"));
    assert_eq!(task.group, "plugins");
}

// ---------------------------------------------------------------------------
// Re-runnability (Fn, not FnOnce)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dynamic_task_rerunnable() {
    let mut ctx = InitContext::new("");
    ctx.register_task("rerun_me", None, |_ctx, _args| {
        Box::pin(async { Ok(()) })
    });

    let mut reg = Registry::new();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    reg.run("rerun_me").await.unwrap();
    reg.run("rerun_me").await.unwrap();
    reg.run("rerun_me").await.unwrap();
}

// ---------------------------------------------------------------------------
// Full InitContext lifecycle (simulates the runner)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_init_context_lifecycle() {
    // 1. Init phase: discover services, register tasks
    let mut ctx = InitContext::new("docker");
    ctx.set_group_name("Docker Services");

    let services = vec!["web", "db", "redis"];
    for svc in &services {
        let svc_name = svc.to_string();
        ctx.register_task(
            &format!("up_{}", svc),
            Some(&format!("Start {} service", svc)),
            move |_ctx, _args| {
                let svc_name = svc_name.clone();
                Box::pin(async move {
                    let _ = svc_name;
                    Ok(())
                })
            },
        );
    }

    assert_eq!(ctx.group_name(), "Docker Services");

    // 2. Post-init: drain tasks into registry
    let mut reg = Registry::from_inventory();
    for task in ctx.drain_tasks() {
        reg.register(task);
    }

    // 3. Tasks are available and runnable
    reg.run("up_web").await.unwrap();
    reg.run("up_db").await.unwrap();
    reg.run("up_redis").await.unwrap();

    let names: Vec<&str> = reg.list().iter().map(|t| t.name).collect();
    assert!(names.contains(&"up_web"));
    assert!(names.contains(&"up_db"));
    assert!(names.contains(&"up_redis"));
}
