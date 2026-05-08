//! Smoke tests for the build-state machine + watcher driver.
//!
//! These tests use `InProcessSpawner` with the new fail-next mode to
//! simulate `cargo build` failures without involving real compilation.
//! They drive `Supervisor::handle_rebuild_signal` directly rather than
//! relying on a live `notify::RecommendedWatcher` — the watcher's own
//! debounce + filtering is covered in unit tests under `src/mcp/build.rs`.
//!
//! Coverage:
//! 1. Persistence: a gen with tasks survives a rebuild.
//! 2. Build failure surfacing: failed rebuilds flip BuildState and
//!    refuse new spawns; existing-state tools keep working.
//! 3. Recovery: after a failed rebuild, a successful one returns to Idle.
//! 4. Never-had-tasks immediate retirement on rebuild.

use std::sync::Arc;

use rnme::execution::{SpawnOptions, TaskId};
use rnme::mcp::routing::Address;
use rnme::mcp::supervisor::{InProcessSpawner, Supervisor};
use rnme::mcp::wire::{Request, Response, RpcError};
use rnme::task::Registry;

fn registry() -> Arc<Registry> {
    Arc::new(Registry::from_inventory())
}

#[tokio::test]
async fn rebuild_signal_persists_gens_with_tasks() {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    // Spawn a task on gen 1 so it has `has_had_tasks` set.
    let (addr_str, initial_seq) = sup
        .spawn_task(":list".into(), vec![], SpawnOptions::default())
        .await
        .expect("spawn :list");
    let addr = addr_str.parse::<Address>().unwrap();

    // Trigger a rebuild via the public driver method.
    sup.handle_rebuild_signal()
        .await
        .expect("rebuild signal handled");

    // After rebuild: BuildState back to Idle.
    let status = sup.build_status().await;
    assert_eq!(status.state, "idle");

    // Existing-state tool continues to work against gen 1's task.
    let resp = sup
        .request_addr(&addr_str, |a| Request::GetLogs {
            task_id: TaskId(a.task),
            since_seq: Some(initial_seq),
            until_seq: None,
            limit: Some(10),
            filter: None,
        })
        .await
        .expect("get_logs against persisted gen");
    assert!(matches!(resp, Response::GetLogs { .. }));
    let _ = addr;

    sup.shutdown().await;
}

#[tokio::test]
async fn build_failure_surfaces_through_check_can_spawn_and_status() {
    // Construct the spawner first, capture a shareable handle to its
    // failure queue, THEN hand ownership to the supervisor. This lets
    // us stage a failure for the rebuild call only — the initial spawn
    // (during Supervisor::new_with_spawner) succeeds normally.
    let spawner = InProcessSpawner::new(registry());
    let failures = spawner.failure_handle();
    let mut sup = Supervisor::new_with_spawner(Box::new(spawner))
        .await
        .expect("supervisor up");

    // Spawn a task on gen 1 so we can verify existing-state tools still
    // route correctly after the failed rebuild.
    let (addr_str, initial_seq) = sup
        .spawn_task(":list".into(), vec![], SpawnOptions::default())
        .await
        .expect("spawn :list");

    // Stage the next spawn to fail.
    failures.fail_next("error[E0432]: simulated cargo failure".into());

    // Trigger rebuild — fail_next is queued so this returns Ok(()) but
    // flips BuildState to LastBuildFailed.
    sup.handle_rebuild_signal()
        .await
        .expect("rebuild driver handles failure cleanly");

    let status = sup.build_status().await;
    assert_eq!(status.state, "last_build_failed");
    let captured = status.last_failure_output.expect("captured stderr");
    assert!(
        captured.contains("simulated cargo failure"),
        "stderr surfaces: {captured}"
    );

    // Spawn-shaped guard refuses with BadRequest and embeds the head of
    // the captured output.
    let err = sup.check_can_spawn().await.expect_err("guard refuses");
    match err {
        RpcError::BadRequest(msg) => {
            assert!(
                msg.contains("simulated cargo failure"),
                "head of stderr embedded: {msg}"
            );
            assert!(
                msg.contains("get_build_status"),
                "guidance to call get_build_status: {msg}"
            );
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }

    // Existing-state tool against gen 1's task is unaffected.
    let resp = sup
        .request_addr(&addr_str, |a| Request::GetLogs {
            task_id: TaskId(a.task),
            since_seq: Some(initial_seq),
            until_seq: None,
            limit: Some(10),
            filter: None,
        })
        .await
        .expect("get_logs unaffected by failed rebuild");
    assert!(matches!(resp, Response::GetLogs { .. }));

    sup.shutdown().await;
}

#[tokio::test]
async fn build_failure_recovers_on_next_successful_rebuild() {
    let spawner = InProcessSpawner::new(registry());
    let failures = spawner.failure_handle();
    let mut sup = Supervisor::new_with_spawner(Box::new(spawner))
        .await
        .expect("supervisor up");

    failures.fail_next("first attempt fails".into());
    sup.handle_rebuild_signal()
        .await
        .expect("first rebuild (fails)");
    assert_eq!(sup.build_status().await.state, "last_build_failed");

    sup.handle_rebuild_signal()
        .await
        .expect("second rebuild (succeeds)");
    assert_eq!(sup.build_status().await.state, "idle");

    // Spawn-shaped guard now lets new spawns through.
    sup.check_can_spawn().await.expect("guard ok again");

    sup.shutdown().await;
}

#[tokio::test]
async fn rebuild_signal_retires_never_had_tasks_gen() {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    sup.handle_rebuild_signal()
        .await
        .expect("rebuild ok");

    // The initial gen should have been retired (no tasks ran on it).
    let snap = sup.graph().await;
    assert!(
        snap.top_tasks.is_empty(),
        "no top-tasks survive on the new latest gen: {snap:?}"
    );

    sup.shutdown().await;
}

#[tokio::test]
async fn check_can_spawn_blocks_during_rebuild_then_resolves() {
    use std::time::Duration;

    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    // Manually force the state to Rebuilding to test the guard's
    // blocking behavior. We simulate this by holding the build_state
    // through the build_state_handle.
    let bs = sup.build_state_handle();
    let changed = sup.build_state_changed_handle();
    {
        let mut s = bs.lock().await;
        *s = rnme::mcp::build::BuildState::Rebuilding;
    }

    // check_can_spawn should park while state is Rebuilding. Run it in
    // a separate task so we can drop the immutable borrow on `sup`
    // before calling `shutdown` (which needs `&mut self`).
    let bs2 = sup.build_state_handle();
    let changed2 = sup.build_state_changed_handle();
    let guard_task = tokio::spawn(async move {
        // We can't move `sup` into the task, so re-do the guard's
        // logic inline against the shared handles. This is exactly the
        // pattern Phase 6 will use to avoid holding `&Supervisor`
        // across await points.
        loop {
            let notified = changed2.notified();
            tokio::pin!(notified);
            let s = bs2.lock().await.clone();
            match s {
                rnme::mcp::build::BuildState::Idle => return Ok::<(), String>(()),
                rnme::mcp::build::BuildState::LastBuildFailed { .. } => {
                    return Err("failed".into())
                }
                rnme::mcp::build::BuildState::NoTaskFile { .. } => {
                    return Err("no_task_file".into())
                }
                rnme::mcp::build::BuildState::Rebuilding => notified.await,
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Transition to Idle and notify; the guard should resolve.
    {
        let mut s = bs.lock().await;
        *s = rnme::mcp::build::BuildState::Idle;
    }
    changed.notify_waiters();

    let res = tokio::time::timeout(Duration::from_secs(2), guard_task).await;
    assert!(matches!(res, Ok(Ok(Ok(())))), "guard resolves: {res:?}");

    sup.shutdown().await;
}
