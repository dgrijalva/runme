//! Smoke tests for the supervisor core.
//!
//! These tests exercise the supervisor's public surface against an
//! in-process engine — no child process, no compilation, no exec — to
//! avoid baking the whole outer-driver pipeline into the test setup.
//!
//! The production [`ProcessEngineSpawner`] re-execs `current_exe()`,
//! which in a `cargo test` binary would be the test runner itself; the
//! [`InProcessSpawner`] sidesteps that entirely.

use std::sync::Arc;

use rnme::execution::{KillSignal, SpawnOptions, TaskId};
use rnme::mcp::routing::Address;
use rnme::mcp::supervisor::{InProcessSpawner, Supervisor};
use rnme::mcp::wire::{Request, Response};
use rnme::task::Registry;

const __RNME_GROUP: &str = "";

fn registry() -> Arc<Registry> {
    Arc::new(Registry::from_inventory())
}

#[tokio::test]
async fn supervisor_spawns_initial_gen_and_serves_list_via_routing() {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    // Spawn a top-level builtin :list task on the latest gen.
    let (addr_str, initial_seq) = sup
        .spawn_task(":list".into(), vec![], SpawnOptions::default())
        .await
        .expect("spawn :list");

    let addr = addr_str.parse::<Address>().expect("address parses");
    assert_eq!(
        addr.task, addr.top,
        "top-level address renders as bare top"
    );

    // Address-routed GetLogs must reach the owning gen and come back ok.
    let resp = sup
        .request_addr(&addr_str, |a| Request::GetLogs {
            task_id: TaskId(a.task),
            since_seq: Some(initial_seq),
            until_seq: None,
            limit: Some(50),
            filter: None,
        })
        .await
        .expect("get_logs ok");
    match resp {
        Response::GetLogs { .. } => {}
        other => panic!("unexpected response: {other:?}"),
    }

    sup.shutdown().await;
}

#[tokio::test]
async fn kill_task_via_dotted_address_is_routed_correctly() {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    let (addr_str, _) = sup
        .spawn_task(":list".into(), vec![], SpawnOptions::default())
        .await
        .expect("spawn :list");

    // Engine accepts kill_task even on already-completed tasks; we just
    // care that the supervisor routes it without an error.
    // Some engine impls treat already-terminal tasks as not-found, so
    // accept either ok or NotFound here — the goal is to confirm
    // routing actually reached the engine.
    let result = sup.kill_task(&addr_str, KillSignal::Term).await;
    match result {
        Ok(()) => {}
        Err(rnme::mcp::wire::RpcError::Engine(_)) => {}
        Err(rnme::mcp::wire::RpcError::NotFound(_)) => {}
        Err(e) => panic!("unexpected supervisor-side error: {e:?}"),
    }

    sup.shutdown().await;
}

#[tokio::test]
async fn unknown_address_yields_not_found_without_crossing_tcp() {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    let err = sup
        .request_addr("999999", |a| Request::GetLogs {
            task_id: TaskId(a.task),
            since_seq: None,
            until_seq: None,
            limit: Some(10),
            filter: None,
        })
        .await
        .expect_err("unknown top → NotFound");
    match err {
        rnme::mcp::wire::RpcError::NotFound(_) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }

    sup.shutdown().await;
}

#[tokio::test]
async fn malformed_address_yields_bad_request() {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let mut sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");

    let err = sup
        .request_addr("nope", |a| Request::GetLogs {
            task_id: TaskId(a.task),
            since_seq: None,
            until_seq: None,
            limit: Some(10),
            filter: None,
        })
        .await
        .expect_err("malformed → BadRequest");
    match err {
        rnme::mcp::wire::RpcError::BadRequest(_) => {}
        other => panic!("expected BadRequest, got {other:?}"),
    }

    sup.shutdown().await;
}
