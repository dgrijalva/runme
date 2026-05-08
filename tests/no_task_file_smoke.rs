//! Smoke tests for the `BuildState::NoTaskFile` degraded-mode path.
//!
//! When `rnme --mcp` is launched in a directory with no `RUNME.rs` (or any
//! ancestor `RUNME.rs`), the supervisor must:
//!
//! 1. Start cleanly — never crash.
//! 2. Surface a friendly "no RUNME.rs" error from spawn-shaped tool calls.
//! 3. Return an empty graph for read-only tools.
//! 4. Transition to normal operation when a `RUNME.rs` is later created.
//!
//! This is the supervisor-side guarantee that lets the MCP server be
//! installed globally (Claude Code inheriting cwd from wherever it was
//! launched) without crashing on first contact.

use std::sync::Arc;

use rnme::execution::SpawnOptions;
use rnme::mcp::supervisor::{InProcessSpawner, Supervisor};
use rnme::mcp::wire::RpcError;
use rnme::task::Registry;
use tempfile::TempDir;

const __RNME_GROUP: &str = "";

fn registry() -> Arc<Registry> {
    Arc::new(Registry::from_inventory())
}

/// Construct a supervisor whose cwd is a fresh tempdir with no RUNME.rs
/// in it (or in any ancestor — tempdirs live under /tmp/T/, which has
/// no RUNME.rs).
async fn boot_in_empty_dir() -> (Supervisor, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let sup = Supervisor::new_with_spawner_and_cwd(spawner, dir.path().to_path_buf())
        .await
        .expect("supervisor up in empty dir");
    (sup, dir)
}

#[tokio::test]
async fn supervisor_starts_in_no_task_file_when_cwd_has_no_runme() {
    let (sup, dir) = boot_in_empty_dir().await;
    let status = sup.build_status().await;
    assert_eq!(status.state, "no_task_file");
    let searched = status.searched_from.expect("searched_from set");
    assert!(
        searched.contains(&dir.path().display().to_string()),
        "searched_from points at cwd: {searched}"
    );
    assert!(
        sup.graph().await.top_tasks.is_empty(),
        "no live gens → empty graph"
    );
}

#[tokio::test]
async fn spawn_task_returns_friendly_error_in_no_task_file() {
    let (mut sup, _dir) = boot_in_empty_dir().await;
    let err = sup
        .check_can_spawn()
        .await
        .expect_err("guard refuses in NoTaskFile");
    match err {
        RpcError::BadRequest(msg) => {
            assert!(
                msg.contains("no RUNME.rs"),
                "friendly message: {msg}"
            );
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }

    // spawn_task itself returns Internal because there's no live gen, but
    // the caller should always go through check_can_spawn first; that's
    // the contract enforced at the tool layer.
    let res = sup
        .spawn_task("anything".into(), vec![], SpawnOptions::default())
        .await;
    assert!(res.is_err(), "spawn_task fails with no live gen");
}

#[tokio::test]
async fn list_tasks_returns_friendly_error_in_no_task_file() {
    let (sup, _dir) = boot_in_empty_dir().await;
    let err = sup.list_tasks().await.expect_err("list_tasks refused");
    match err {
        RpcError::BadRequest(msg) => {
            assert!(msg.contains("no RUNME.rs"), "friendly message: {msg}");
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn get_graph_returns_empty_in_no_task_file() {
    let (sup, _dir) = boot_in_empty_dir().await;
    let snap = sup.graph().await;
    assert!(snap.top_tasks.is_empty(), "graph empty in NoTaskFile mode");
}

#[tokio::test]
async fn rebuild_signal_without_runme_stays_in_no_task_file() {
    let (mut sup, _dir) = boot_in_empty_dir().await;
    sup.handle_rebuild_signal()
        .await
        .expect("rebuild signal handled cleanly");
    assert_eq!(sup.build_status().await.state, "no_task_file");
}

#[tokio::test]
async fn creating_runme_then_signaling_transitions_to_idle() {
    let (mut sup, dir) = boot_in_empty_dir().await;
    assert_eq!(sup.build_status().await.state, "no_task_file");

    // Drop a (minimal) RUNME.rs into the cwd. Content doesn't matter
    // because the InProcessSpawner doesn't actually compile anything;
    // it just needs discover() to find a file so the rebuild driver
    // takes the spawn path.
    std::fs::write(dir.path().join("RUNME.rs"), "// hi\n").expect("write RUNME.rs");

    sup.handle_rebuild_signal()
        .await
        .expect("rebuild succeeds once RUNME.rs exists");

    assert_eq!(sup.build_status().await.state, "idle");

    // Spawn-shaped guard now lets us through.
    sup.check_can_spawn()
        .await
        .expect("guard ok once a gen is live");

    // And we can spawn a builtin against the new gen.
    let _ = sup
        .spawn_task(":list".into(), vec![], SpawnOptions::default())
        .await
        .expect("spawn :list against newly-live gen");

    sup.shutdown().await;
}

#[tokio::test]
async fn deleting_runme_then_signaling_returns_to_no_task_file() {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("RUNME.rs"), "// hi\n").expect("write RUNME.rs");
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner_and_cwd(spawner, dir.path().to_path_buf())
        .await
        .expect("supervisor up with RUNME.rs");
    assert_eq!(sup.build_status().await.state, "idle");

    // Remove the file and trigger a rebuild — should flip back to NoTaskFile.
    std::fs::remove_file(dir.path().join("RUNME.rs")).expect("remove RUNME.rs");
    sup.handle_rebuild_signal()
        .await
        .expect("rebuild signal handled");
    assert_eq!(sup.build_status().await.state, "no_task_file");

    sup.shutdown().await;
}
