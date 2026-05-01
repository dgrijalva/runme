//! In-process fixture tasks for integration testing.
//!
//! These are NOT RUNME.rs files — they're regular test tasks that exercise
//! common patterns. Other test files import and run these via
//! `Registry::from_inventory()`.
//!
//! No assertions here — just task definitions. Tests in separate files
//! (tasks #6, #7) will exercise these.

use rnme::prelude::*;

const __RNME_GROUP: &str = "";

// ============================================================
// Basic success / failure
// ============================================================

/// A task that succeeds immediately.
#[rnme::task]
async fn succeed(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}

/// A task that fails with the default exit code (1).
#[rnme::task]
async fn fail_default(_ctx: &TaskContext) -> TaskResult {
    Err("task failed".into())
}

/// A task that fails with a specific exit code (42).
#[rnme::task]
async fn fail_with_code(_ctx: &TaskContext) -> TaskResult {
    Err(TaskError::from("task failed with code 42").with_code(42))
}

// ============================================================
// Arguments
// ============================================================

/// A task that logs its arguments via info!().
#[rnme::task]
async fn echo_args(_ctx: &TaskContext, message: String) -> TaskResult {
    info!("echo_args: message={}", message);
    Ok(())
}

// ============================================================
// Process execution
// ============================================================

/// A task that uses ctx.exec() to run a simple command.
#[rnme::task]
async fn spawn_echo(ctx: &TaskContext) -> TaskResult {
    ctx.exec("echo hello from spawn_echo").await?.ok()?;
    Ok(())
}

/// A task that tries to exec a nonexistent command.
#[rnme::task]
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
#[rnme::task]
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
#[rnme::task]
async fn invoke_other(ctx: &TaskContext) -> TaskResult {
    ctx.run("succeed", &[]).await?;
    Ok(())
}

// ============================================================
// Process lifecycle
// ============================================================

/// A task that spawns a long-running process and returns immediately.
/// Used to test that stop_all() cleans up spawned processes.
///
/// With the new ProcessHandle::Drop semantics (handle drop sends SIGTERM),
/// we explicitly detach via `tokio::spawn` so the handle outlives the
/// task body — exercising the `stop_all` cleanup path instead of the
/// drop-kills path.
#[rnme::task]
async fn spawn_sleeper(ctx: &TaskContext) -> TaskResult {
    let handle = ctx.spawn("sleep 300").await?;
    tokio::spawn(async move {
        let _h = handle;
        std::future::pending::<()>().await;
    });
    Ok(())
}

// ============================================================
// UI mode hints (mode = cli|tui via macro attr)
// ============================================================

/// Mode hint: cli (bare ident).
#[rnme::task(mode = cli)]
async fn mode_hint_cli(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}

/// Mode hint: tui (bare ident).
#[rnme::task(mode = tui)]
async fn mode_hint_tui(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}

/// Mode hint: cli (string literal).
#[rnme::task(mode = "cli")]
async fn mode_hint_cli_str(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}
