//! In-process integration tests for the rnme task system.
//!
//! Tests exercise fixture tasks (from fixtures.rs) via `Registry::from_inventory()`
//! and `run_with_args()`. Each test verifies one specific behavior.

use std::sync::Arc;

use rnme::cli::{resolve_ui_mode, UiMode};
use rnme::prelude::*;
use nix::sys::signal;
use nix::unistd::Pid;

mod common;
#[path = "fixtures.rs"]
mod fixtures;

const __RNME_GROUP: &str = "";

// ============================================================
// Priority 1: Task error propagation
// ============================================================

/// Verify that a task returning Err propagates through Registry,
/// and the TaskError message is accessible.
#[tokio::test]
async fn test_error_propagates_through_registry() {
    let reg = Registry::from_inventory();
    let result = reg.run_with_args("fail_default", &[]).await;
    assert!(result.is_err(), "fail_default should return Err");
    let err = result.unwrap_err();
    assert_eq!(err.to_string(), "task failed");
}

/// Verify that a task returning Err with a specific exit code (42)
/// propagates the ExitHint::Code correctly.
#[tokio::test]
async fn test_error_with_exit_code() {
    let reg = Registry::from_inventory();
    let result = reg.run_with_args("fail_with_code", &[]).await;
    assert!(result.is_err(), "fail_with_code should return Err");
    let err = result.unwrap_err();
    assert_eq!(err.exit_code(), 42);
    assert_eq!(err.hint(), &ExitHint::Code(42));
    assert_eq!(err.to_string(), "task failed with code 42");
}

/// Verify that a successful task returns Ok.
#[tokio::test]
async fn test_success_returns_ok() {
    let reg = Registry::from_inventory();
    let result = reg.run_with_args("succeed", &[]).await;
    assert!(result.is_ok(), "succeed task should return Ok");
}

// ============================================================
// Priority 1: resolve_ui_mode() — pure function tests
// ============================================================

/// Explicit --ui tui with terminal → Tui.
#[test]
fn test_resolve_ui_mode_explicit_tui_with_terminal() {
    let mode = resolve_ui_mode(Some(UiMode::Tui), None, true);
    assert!(matches!(mode, UiMode::Tui));
}

/// Explicit --ui tui without terminal → falls back to Cli.
#[test]
fn test_resolve_ui_mode_explicit_tui_no_terminal() {
    let mode = resolve_ui_mode(Some(UiMode::Tui), None, false);
    assert!(matches!(mode, UiMode::Cli));
}

/// Explicit --ui cli always → Cli regardless of terminal.
#[test]
fn test_resolve_ui_mode_explicit_cli() {
    let with_term = resolve_ui_mode(Some(UiMode::Cli), None, true);
    let without_term = resolve_ui_mode(Some(UiMode::Cli), None, false);
    assert!(matches!(with_term, UiMode::Cli));
    assert!(matches!(without_term, UiMode::Cli));
}

/// Explicit --ui agent always → Agent regardless of terminal.
#[test]
fn test_resolve_ui_mode_explicit_agent() {
    let with_term = resolve_ui_mode(Some(UiMode::Agent), None, true);
    let without_term = resolve_ui_mode(Some(UiMode::Agent), None, false);
    assert!(matches!(with_term, UiMode::Agent));
    assert!(matches!(without_term, UiMode::Agent));
}

/// Explicit flag overrides task hint.
#[test]
fn test_resolve_ui_mode_explicit_overrides_hint() {
    let mode = resolve_ui_mode(Some(UiMode::Cli), Some(UiHint::Tui), true);
    assert!(matches!(mode, UiMode::Cli));
}

/// Task hint Cli → Cli regardless of terminal.
#[test]
fn test_resolve_ui_mode_hint_cli() {
    let with_term = resolve_ui_mode(None, Some(UiHint::Cli), true);
    let without_term = resolve_ui_mode(None, Some(UiHint::Cli), false);
    assert!(matches!(with_term, UiMode::Cli));
    assert!(matches!(without_term, UiMode::Cli));
}

/// Task hint Tui with terminal → Tui.
#[test]
fn test_resolve_ui_mode_hint_tui_with_terminal() {
    let mode = resolve_ui_mode(None, Some(UiHint::Tui), true);
    assert!(matches!(mode, UiMode::Tui));
}

/// Task hint Tui without terminal → falls back to Cli.
#[test]
fn test_resolve_ui_mode_hint_tui_no_terminal() {
    let mode = resolve_ui_mode(None, Some(UiHint::Tui), false);
    assert!(matches!(mode, UiMode::Cli));
}

/// No explicit flag, no hint, terminal available → Tui.
#[test]
fn test_resolve_ui_mode_default_with_terminal() {
    let mode = resolve_ui_mode(None, None, true);
    assert!(matches!(mode, UiMode::Tui));
}

/// No explicit flag, no hint, no terminal → Cli.
#[test]
fn test_resolve_ui_mode_default_no_terminal() {
    let mode = resolve_ui_mode(None, None, false);
    assert!(matches!(mode, UiMode::Cli));
}

// ============================================================
// Priority 2: Init hooks
// ============================================================

/// Define an init hook that sets the group name. Verify it runs
/// when we manually iterate inventory and call it.
#[rnme::init]
fn test_init_hook(ctx: &mut InitContext) {
    ctx.set_group_name("Integration Test Group");
}

#[test]
fn test_init_hook_runs_and_sets_group_name() {
    // Manually collect and run InitDefs from inventory, mirroring what
    // the generated runner binary does (see codegen.rs).
    let inits: Vec<&InitDef> = inventory::iter::<InitDef>.into_iter().collect();

    // Find our init hook (group == "" since __RNME_GROUP is "")
    let our_init = inits
        .iter()
        .find(|init| {
            let mut ctx = InitContext::new("default");
            (init.func)(&mut ctx);
            ctx.group_name() == "Integration Test Group"
        });

    assert!(
        our_init.is_some(),
        "should find an init hook that sets group name to 'Integration Test Group'"
    );
}

// ============================================================
// Priority 2: Cross-task invocation via ctx.run()
// ============================================================

/// Task A invokes task B (the "succeed" fixture) via ctx.run().
/// Verify B runs successfully.
#[tokio::test]
async fn test_cross_task_invocation() {
    let reg = Arc::new(Registry::from_inventory());
    let result = reg
        .run_with_registry("invoke_other", &[], &reg)
        .await;
    assert!(result.is_ok(), "invoke_other should succeed (it calls succeed)");
}

/// Cross-task invocation where inner task fails should propagate the error.
/// Define a helper task that calls fail_default via ctx.run().
#[rnme::task(desc = "Invokes a failing task")]
async fn invoke_failing(ctx: &TaskContext) -> TaskResult {
    ctx.run("fail_default", &[]).await?;
    Ok(())
}

#[tokio::test]
async fn test_cross_task_error_propagation() {
    let reg = Arc::new(Registry::from_inventory());
    let result = reg
        .run_with_registry("invoke_failing", &[], &reg)
        .await;
    assert!(result.is_err(), "invoke_failing should propagate fail_default's error");
    let err = result.unwrap_err();
    assert_eq!(err.to_string(), "task failed");
}

// ============================================================
// Priority 2: Output capture from ctx.exec()
// ============================================================

/// Run the spawn_echo fixture which calls ctx.exec("echo hello from spawn_echo").
/// Verify the output appears in the LogStore (exec now goes through spawn, so
/// output flows via SpawnEvent → monitor_spawns → LogStore).
#[tokio::test]
async fn test_output_capture_from_exec() {
    use rnme::execution::{LaunchConfig, TaskExecution};

    let reg = Arc::new(Registry::from_inventory());
    let task = reg.get("spawn_echo").unwrap();
    let mut exec = TaskExecution::new();
    exec.set_registry(reg);
    exec.launch(task, vec![], LaunchConfig::default());
    exec.wait().await;

    // Give the monitor_spawns forwarder a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let store = exec.log_store().lock().await;
    let entries = store.compose_owned();
    let found = entries.iter().any(|e| e.raw.contains("hello from spawn_echo"));
    assert!(found, "LogStore should contain 'hello from spawn_echo'");
}

// ============================================================
// Priority 3: Log levels
// ============================================================

// NOTE: Tracing macros (info!, warn!, error!) in task functions are captured
// via the tracing layer, which requires a subscriber to be installed. In the
// minimal test context (no TUI, no tracing subscriber), these events do not
// appear in the TaskContext output buffer. The log_levels fixture tests
// correctness of the task definition itself; tracing capture requires the full
// TUI or CLI runtime and is out of scope for in-process tests.

// ============================================================
// Priority 3: Task name resolution
// ============================================================

/// resolve() with an exact name finds the task.
#[test]
fn test_resolve_exact_name() {
    let reg = Registry::from_inventory();
    let task = reg.resolve("succeed");
    assert!(task.is_ok(), "resolve('succeed') should find the task");
    assert_eq!(task.unwrap().name, "succeed");
}

/// resolve() with an unknown name returns an error.
#[test]
fn test_resolve_unknown_name() {
    let reg = Registry::from_inventory();
    let result = reg.resolve("nonexistent_task_xyz");
    assert!(result.is_err());
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert!(
        err.to_string().contains("unknown task"),
        "error should mention 'unknown task': {}",
        err,
    );
}

/// resolve() for a nonexistent command via exec returns an error.
#[tokio::test]
async fn test_exec_nonexistent_command() {
    let reg = Registry::from_inventory();
    let result = reg.run_with_args("fail_spawn", &[]).await;
    assert!(result.is_err(), "fail_spawn should return Err");
}

// ============================================================
// Task discovery + invocation (ctx.tasks() → ctx.run())
// ============================================================

#[rnme::task(desc = "Step A")]
async fn step_a(_ctx: &TaskContext) -> TaskResult {
    info!("step_a ran");
    Ok(())
}

#[rnme::task(desc = "Step B")]
async fn step_b(_ctx: &TaskContext) -> TaskResult {
    info!("step_b ran");
    Ok(())
}

#[rnme::task(desc = "Step C — fails")]
async fn step_c(_ctx: &TaskContext) -> TaskResult {
    Err("step_c failed".into())
}

/// Discovers tasks matching "step_*", runs each, collects results.
#[rnme::task(desc = "Run all steps")]
async fn run_discovered_steps(ctx: &TaskContext) -> TaskResult {
    let query = ctx.tasks().expect("registry should be injected");
    let steps = query.matching("step_*");
    assert!(!steps.is_empty(), "should discover step tasks");

    let mut ran = Vec::new();
    for step in &steps {
        let result = ctx.run(step.name, &[]).await;
        ran.push((step.name.to_string(), result.is_ok()));
    }

    // Verify we found and ran the expected tasks
    let names: Vec<&str> = ran.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"step_a"), "should have found step_a");
    assert!(names.contains(&"step_b"), "should have found step_b");
    assert!(names.contains(&"step_c"), "should have found step_c");

    // step_a and step_b succeed, step_c fails
    for (name, ok) in &ran {
        match name.as_str() {
            "step_a" | "step_b" => assert!(ok, "{} should succeed", name),
            "step_c" => assert!(!ok, "{} should fail", name),
            _ => {}
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_discover_and_run_tasks() {
    let reg = Arc::new(Registry::from_inventory());
    let result = reg
        .run_with_registry("run_discovered_steps", &[], &reg)
        .await;
    assert!(result.is_ok(), "coordinator should succeed: {:?}", result);
}

/// Discovers tasks matching a pattern and runs only the ones that match.
#[rnme::task(desc = "Run matching steps selectively")]
async fn run_matching_steps(ctx: &TaskContext) -> TaskResult {
    let query = ctx.tasks().expect("registry should be injected");

    // Only run step_a and step_b (skip step_c which fails)
    let steps = query.matching("step_[ab]");
    assert_eq!(steps.len(), 2, "should match exactly step_a and step_b");

    for step in &steps {
        ctx.run(step.name, &[]).await?;
    }
    Ok(())
}

#[tokio::test]
async fn test_discover_and_run_matching_tasks() {
    let reg = Arc::new(Registry::from_inventory());
    let result = reg
        .run_with_registry("run_matching_steps", &[], &reg)
        .await;
    assert!(result.is_ok(), "selective run should succeed: {:?}", result);
}

// ============================================================
// Priority 1: Process cleanup on exit
// ============================================================

/// Verify that stop_all() kills spawned processes and their children.
///
/// Spawns a long-running process (sleep 300) via ctx.spawn(), confirms
/// it is alive, then calls ctx.stop_all() and confirms it is dead.
#[tokio::test]
async fn test_stop_all_kills_spawned_processes() {
    let reg = Registry::from_inventory();
    let task = reg.get("spawn_sleeper").unwrap();
    let ctx = TaskContext::new(task.name);
    let result = task.func.call(&ctx, &[]).await;
    assert!(result.is_ok(), "spawn_sleeper should succeed");

    // Collect the PGIDs that were tracked
    let pgids = {
        let guard = ctx.spawned_pgids().await;
        guard.clone()
    };
    assert!(!pgids.is_empty(), "should have tracked at least one PGID");

    // Verify the process group is alive (signal 0 = existence check)
    for &pgid in &pgids {
        let alive = signal::killpg(Pid::from_raw(pgid), None);
        assert!(alive.is_ok(), "PGID {} should be alive before stop_all", pgid);
    }

    // Stop all spawned processes
    ctx.stop_all(std::time::Duration::from_secs(2)).await;

    // Give the OS a moment to reap
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the process group is dead
    for &pgid in &pgids {
        let dead = signal::killpg(Pid::from_raw(pgid), None);
        assert!(dead.is_err(), "PGID {} should be dead after stop_all", pgid);
    }
}
