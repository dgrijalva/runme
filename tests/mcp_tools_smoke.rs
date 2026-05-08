//! Smoke tests for the rmcp tool surface (Phase 6 / `t6.i-mcp-tools`).
//!
//! These tests exercise the tool methods on [`McpServer`] directly without
//! standing up an rmcp transport. The transport layer is rmcp's
//! responsibility; we only need to assert that our tool implementations
//! produce correct structured outputs and route through the supervisor
//! correctly.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rnme::mcp::supervisor::{InProcessSpawner, Supervisor};
use rnme::mcp::tools::{
    GetLogsParams, GetTaskParams, InstallSkillsParams, McpServer, SpawnTaskParams,
};
use rnme::task::Registry;

const __RNME_GROUP: &str = "";

fn registry() -> Arc<Registry> {
    Arc::new(Registry::from_inventory())
}

async fn boot() -> McpServer {
    let spawner = Box::new(InProcessSpawner::new(registry()));
    let sup = Supervisor::new_with_spawner(spawner)
        .await
        .expect("supervisor up");
    McpServer::new(sup)
}

/// Spawn a `:list` task and wait for the supervisor's cached snapshot to
/// see it. Polling matches what `run_task` does internally.
async fn wait_for_top(server: &McpServer, top: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let sup = server.supervisor().lock_owned().await;
        let snap = sup.graph().await;
        if snap.top_tasks.iter().any(|t| {
            t.id.parse::<rnme::mcp::routing::Address>()
                .map(|a| a.top == top)
                .unwrap_or(false)
        }) {
            return;
        }
        drop(sup);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("top {top} never appeared in supervisor graph snapshot");
}

#[tokio::test]
async fn list_tasks_returns_structured_payload() {
    let server = boot().await;
    let result = server.list_tasks().await.expect("list_tasks ok");
    let value = result
        .structured_content
        .clone()
        .expect("structured content present");
    // MCP `structuredContent` requires a top-level object; we wrap the
    // task list under `tasks`.
    assert!(value.is_object(), "expected object, got {value:?}");
    let arr = value
        .get("tasks")
        .and_then(|v| v.as_array())
        .expect("expected `tasks` array on the structuredContent object");
    assert!(!arr.is_empty(), "expected at least one task (`:list` builtin)");
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|v| v.get("name")?.as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("list")),
        "expected a `list` task in {names:?}"
    );
}

#[tokio::test]
async fn spawn_task_returns_id_and_initial_seq() {
    let server = boot().await;
    let result = server
        .spawn_task(Parameters(SpawnTaskParams {
            name: ":list".into(),
            args: vec![],
            timeout_seconds: None,
        }))
        .await
        .expect("spawn ok");
    let value = result
        .structured_content
        .clone()
        .expect("structured content present");
    let task_id = value
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id string");
    assert!(!task_id.is_empty());
    let _seq = value
        .get("initial_seq")
        .and_then(|v| v.as_u64())
        .expect("initial_seq u64");
}

#[tokio::test]
async fn get_task_renders_report_after_spawn() {
    let server = boot().await;
    let spawn = server
        .spawn_task(Parameters(SpawnTaskParams {
            name: ":list".into(),
            args: vec![],
            timeout_seconds: None,
        }))
        .await
        .expect("spawn ok");
    let value = spawn.structured_content.clone().unwrap();
    let task_id = value.get("task_id").unwrap().as_str().unwrap().to_string();
    let top = task_id
        .parse::<rnme::mcp::routing::Address>()
        .unwrap()
        .top;

    wait_for_top(&server, top).await;

    let report = server
        .get_task(Parameters(GetTaskParams {
            id: task_id,
            tail_n: Some(20),
        }))
        .await
        .expect("get_task ok");
    assert!(
        report.starts_with("Task t"),
        "report should start with `Task t`: {report:?}"
    );
}

#[tokio::test]
async fn get_logs_round_trips() {
    let server = boot().await;
    let spawn = server
        .spawn_task(Parameters(SpawnTaskParams {
            name: ":list".into(),
            args: vec![],
            timeout_seconds: None,
        }))
        .await
        .expect("spawn ok");
    let value = spawn.structured_content.clone().unwrap();
    let task_id = value.get("task_id").unwrap().as_str().unwrap().to_string();
    let initial_seq = value.get("initial_seq").unwrap().as_u64().unwrap();

    let result = server
        .get_logs(Parameters(GetLogsParams {
            task_id,
            since_seq: Some(initial_seq),
            until_seq: None,
            limit: Some(50),
            filter: None,
        }))
        .await
        .expect("get_logs ok");
    let value = result.structured_content.clone().expect("structured");
    assert!(value.get("entries").is_some(), "entries missing: {value:?}");
    assert!(value.get("next_seq").is_some(), "next_seq missing: {value:?}");
    assert!(value.get("has_more").is_some(), "has_more missing: {value:?}");
}

#[tokio::test]
async fn get_build_status_idle_initially() {
    let server = boot().await;
    let result = server.get_build_status().await.expect("ok");
    let value = result.structured_content.clone().expect("structured");
    let state = value.get("state").and_then(|v| v.as_str()).unwrap();
    assert_eq!(state, "idle", "expected idle state, got {state}");
}

#[tokio::test]
async fn install_skills_returns_not_implemented() {
    let server = boot().await;
    let err = server
        .install_skills(Parameters(InstallSkillsParams {
            target_dir: "/tmp/rnme-skills-test".into(),
        }))
        .await
        .expect_err("install_skills must error in Phase 6");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Phase 7") || msg.contains("not yet implemented") || msg.contains("install_skills"),
        "expected stub error mentioning Phase 7: {msg}"
    );
}

#[tokio::test]
async fn get_graph_returns_top_tasks_array() {
    let server = boot().await;
    let result = server.get_graph().await.expect("ok");
    let value = result.structured_content.clone().expect("structured");
    let tops = value.get("top_tasks").expect("top_tasks present");
    assert!(tops.is_array(), "top_tasks must be an array");
}

#[tokio::test]
async fn kill_all_acks_when_no_tasks_running() {
    let server = boot().await;
    let result = server.kill_all().await.expect("kill_all ok");
    let value = result.structured_content.clone().expect("structured");
    assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(true));
}
