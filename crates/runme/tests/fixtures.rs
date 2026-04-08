//! In-process fixture tasks for integration testing.
//!
//! These are NOT RUNME.rs files — they're regular test tasks that exercise
//! common patterns. Other test files import and run these via
//! `Registry::from_inventory()`.
//!
//! No assertions here — just task definitions. Tests in separate files
//! (tasks #6, #7) will exercise these.

use runme::prelude::*;

const __RUNME_GROUP: &str = "";

// ============================================================
// Basic success / failure
// ============================================================

/// A task that succeeds immediately.
#[runme::task(desc = "Returns Ok(())")]
async fn succeed(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}

/// A task that fails with the default exit code (1).
#[runme::task(desc = "Returns Err with default exit code")]
async fn fail_default(_ctx: &TaskContext) -> TaskResult {
    Err("task failed".into())
}

/// A task that fails with a specific exit code (42).
#[runme::task(desc = "Returns Err with exit code 42")]
async fn fail_with_code(_ctx: &TaskContext) -> TaskResult {
    Err(TaskError::from("task failed with code 42").with_code(42))
}

// ============================================================
// Arguments
// ============================================================

/// A task that logs its arguments via info!().
#[runme::task(desc = "Logs arguments")]
async fn echo_args(_ctx: &TaskContext, message: String) -> TaskResult {
    info!("echo_args: message={}", message);
    Ok(())
}

// ============================================================
// Process execution
// ============================================================

/// A task that uses ctx.exec() to run a simple command.
#[runme::task(desc = "Runs echo via ctx.exec()")]
async fn spawn_echo(ctx: &TaskContext) -> TaskResult {
    ctx.exec("echo hello from spawn_echo").await?.ok()?;
    Ok(())
}

/// A task that tries to exec a nonexistent command.
#[runme::task(desc = "Tries to run a nonexistent command")]
async fn fail_spawn(ctx: &TaskContext) -> TaskResult {
    let result = ctx.exec("__nonexistent_command_12345").await;
    match result {
        Ok(pr) => {
            pr.ok()?;
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

// ============================================================
// Logging
// ============================================================

/// A task that emits messages at multiple log levels.
#[runme::task(desc = "Emits info, warn, error")]
async fn log_levels(_ctx: &TaskContext) -> TaskResult {
    info!("info message from log_levels");
    warn!("warn message from log_levels");
    error!("error message from log_levels");
    Ok(())
}

// ============================================================
// Cross-task invocation
// ============================================================

/// A task that invokes the "succeed" task via ctx.run().
#[runme::task(desc = "Invokes another task")]
async fn invoke_other(ctx: &TaskContext) -> TaskResult {
    ctx.run("succeed", &[]).await?;
    Ok(())
}

// ============================================================
// Process lifecycle
// ============================================================

/// A task that spawns a long-running process and returns immediately.
/// Used to test that stop_all() cleans up spawned processes.
#[runme::task(desc = "Spawns a sleep process")]
async fn spawn_sleeper(ctx: &TaskContext) -> TaskResult {
    let _handle = ctx.spawn("sleep 300").await?;
    Ok(())
}
