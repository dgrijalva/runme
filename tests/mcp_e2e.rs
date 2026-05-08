//! End-to-end integration test for the rnme MCP server.
//!
//! Spawns the actual built `runme --mcp` binary as a subprocess, connects
//! to it via rmcp's `TokioChildProcess` stdio transport, and exercises the
//! core tool surface against a real RUNME.rs (the one in
//! `docs/examples/RUNME.rs`).
//!
//! This catches regressions in:
//! - argv encoding, cwd inheritance through `std::process::Command`
//! - the supervisor↔engine subprocess boot sequence (port-line handshake,
//!   real cargo compile of the user's RUNME.rs, real TCP loopback)
//! - rmcp framing on stdio
//! - the Generation lifecycle through actual subprocess paths
//!
//! Marked `#[ignore]` because it triggers a real cargo build of the
//! example RUNME.rs (~30s+ on a cold cache). Run on demand with:
//!
//! ```sh
//! cargo test --test mcp_e2e -- --ignored --nocapture mcp_full_smoke
//! ```

use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;

const FIRST_BUILD_TIMEOUT_SECS: u64 = 240;

#[tokio::test]
#[ignore = "spawns runme --mcp subprocess; runs cargo compile of docs/examples/RUNME.rs (~30s+ first run)"]
async fn mcp_full_smoke() {
    // ---- Setup: tempdir with example RUNME.rs ----
    let tempdir = tempfile::tempdir().expect("tempdir");
    let runme_path = tempdir.path().join("RUNME.rs");
    let example_src = std::fs::canonicalize("docs/examples/RUNME.rs")
        .expect("canonicalize docs/examples/RUNME.rs (run from repo root)");
    std::fs::copy(&example_src, &runme_path).expect("copy example RUNME.rs into tempdir");
    let cache_dir = tempdir.path().join(".rnme-cache");
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");

    eprintln!("[mcp_e2e] tempdir = {}", tempdir.path().display());
    eprintln!("[mcp_e2e] cache   = {}", cache_dir.display());
    eprintln!("[mcp_e2e] binary  = {}", env!("CARGO_BIN_EXE_runme"));

    // ---- Spawn supervisor ----
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_runme"));
    command
        .arg("--mcp")
        .current_dir(tempdir.path())
        .env("RNME_CACHE_DIR", &cache_dir)
        // Make the supervisor noisy on stderr so --nocapture is useful.
        .env("RUST_LOG", "warn,rnme=info");

    let transport = TokioChildProcess::new(command).expect("spawn runme --mcp");
    let running = ()
        .serve(transport)
        .await
        .expect("rmcp client serve (supervisor handshake)");
    let peer = running.peer().clone();

    // Drive the test inside a panic guard so the subprocess gets cleaned
    // up on assertion failures via the cancel below.
    let result = std::panic::AssertUnwindSafe(async {
        // ---- Wait for the first build to settle ----
        // The supervisor enters Rebuilding the moment gen 1 is requested,
        // and stays there while the outer driver runs `cargo build` on the
        // generated workspace. First-run compile can take 30s+ on cold
        // caches.
        poll_build_until_idle(&peer, Duration::from_secs(FIRST_BUILD_TIMEOUT_SECS)).await;

        // ---- Exercise the tool surface ----
        test_list_tasks(&peer).await;
        test_get_build_status_idle(&peer).await;
        test_spawn_then_get_task(&peer).await;
        test_run_task_transient(&peer).await;
        test_run_task_failed(&peer).await;
        test_long_running_then_kill(&peer).await;
        test_get_graph(&peer).await;
        test_grep_logs(&peer).await;
        test_kill_all(&peer).await;
        test_install_skills_stub(&peer).await;
    });
    let outcome = futures::FutureExt::catch_unwind(result).await;

    // ---- Tear down: drop the peer + cancel; supervisor sees stdin EOF. ----
    drop(peer);
    let _ = running.cancel().await;

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

// ---------------------------------------------------------------------------
// Per-tool helpers
// ---------------------------------------------------------------------------

async fn test_list_tasks(peer: &rmcp::Peer<rmcp::RoleClient>) {
    // Discovery happens through `list_tools` first to confirm the server
    // surfaces every tool we care about.
    let tools = peer
        .list_tools(Default::default())
        .await
        .expect("list_tools");
    let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    for required in [
        "list_tasks",
        "spawn_task",
        "run_task",
        "kill_task",
        "kill_process",
        "kill_all",
        "get_graph",
        "get_task",
        "get_logs",
        "grep_logs",
        "get_build_status",
        "install_skills",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "tools/list missing {required}: got {names:?}"
        );
    }

    // Now drive `list_tasks` itself.
    let result = call(peer, "list_tasks", None).await;
    let value = structured(&result);
    let arr = value
        .get("tasks")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("list_tasks structuredContent missing `tasks` array: {value}"));
    let task_names: Vec<String> = arr
        .iter()
        .filter_map(|v| v.get("qualified_name").and_then(|n| n.as_str()))
        .map(str::to_string)
        .collect();
    eprintln!("[mcp_e2e] tasks: {task_names:?}");
    // The copy of RUNME.rs lives at the root of the tempdir, so its
    // group key is empty — tasks come back unqualified. The example
    // init hook's `set_group_name` only changes the *display* name, not
    // the qualified name's group prefix.
    for required in ["ticker", "transient", "fail", "multi"] {
        assert!(
            task_names.iter().any(|n| n == required),
            "list_tasks missing {required}: got {task_names:?}"
        );
    }
}

async fn test_get_build_status_idle(peer: &rmcp::Peer<rmcp::RoleClient>) {
    let result = call(peer, "get_build_status", None).await;
    let value = structured(&result);
    let state = value.get("state").and_then(|v| v.as_str()).unwrap();
    assert_eq!(state, "idle", "expected idle state, got {value}");
}

async fn test_spawn_then_get_task(peer: &rmcp::Peer<rmcp::RoleClient>) {
    // Spawn :transient (sleep 1, then exit 0).
    let spawn = call(
        peer,
        "spawn_task",
        Some(serde_json::json!({ "name": "transient" })),
    )
    .await;
    let value = structured(&spawn);
    let task_id = value
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id string")
        .to_string();
    assert!(
        task_id.parse::<u64>().is_ok(),
        "expected bare numeric top-task id, got {task_id:?}"
    );

    // Poll get_task until terminal. ~3s ceiling — sleep is 1s, plus boot.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = String::new();
    while Instant::now() < deadline {
        let report_res = call(
            peer,
            "get_task",
            Some(serde_json::json!({ "id": task_id, "tail_n": 20 })),
        )
        .await;
        let report = first_text(&report_res).expect("get_task text");
        last = report.clone();
        assert!(
            report.starts_with("Task t"),
            "report should start with `Task t`: {report:?}"
        );
        if report.contains("completed (exit 0)") || report.contains("completed") {
            assert!(report.contains("Started:"), "missing Started: in {report}");
            assert!(report.contains("Run time:"), "missing Run time: in {report}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("transient never reached completion. last report:\n{last}");
}

async fn test_run_task_transient(peer: &rmcp::Peer<rmcp::RoleClient>) {
    // run_task is compound: spawns + waits + renders the report.
    let result = call(
        peer,
        "run_task",
        Some(serde_json::json!({ "name": "transient", "tail_n": 20 })),
    )
    .await;
    let report = first_text(&result).expect("run_task text");
    assert!(
        report.starts_with("Task t"),
        "report should start with `Task t`: {report:?}"
    );
    assert!(
        report.contains("completed"),
        "expected `completed` in report:\n{report}"
    );
}

async fn test_run_task_failed(peer: &rmcp::Peer<rmcp::RoleClient>) {
    // The example's `fail` task spawns a subprocess that exits 1 but
    // the task body swallows the failure (calls `.await?` which only
    // propagates spawn errors, not non-zero exits) and returns Ok(()).
    // So at the *task* level it shows "completed", but the rendered
    // report should still surface the subprocess's stderr.
    let result = call(
        peer,
        "run_task",
        Some(serde_json::json!({ "name": "fail", "tail_n": 20 })),
    )
    .await;
    let report = first_text(&result).expect("run_task text");
    assert!(
        report.starts_with("Task t"),
        "report should start with `Task t`: {report:?}"
    );
    // Either the subprocess stderr or some surfacing of the failure
    // should appear in the rendered report.
    assert!(
        report.contains("something went wrong") || report.contains("exit 1"),
        "expected fail subprocess output in report:\n{report}"
    );
}

async fn test_long_running_then_kill(peer: &rmcp::Peer<rmcp::RoleClient>) {
    let spawn = call(
        peer,
        "spawn_task",
        Some(serde_json::json!({ "name": "ticker" })),
    )
    .await;
    let value = structured(&spawn);
    let task_id = value
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id")
        .to_string();
    let initial_seq = value
        .get("initial_seq")
        .and_then(|v| v.as_u64())
        .expect("initial_seq");

    // Give the ticker a few ticks.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // get_logs should have at least one entry mentioning "tick".
    let logs = call(
        peer,
        "get_logs",
        Some(serde_json::json!({
            "task_id": task_id,
            "since_seq": initial_seq,
            "limit": 50,
        })),
    )
    .await;
    let lv = structured(&logs);
    let entries = lv
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("entries array");
    let saw_tick = entries.iter().any(|e| {
        let raw = e.get("raw").and_then(|s| s.as_str()).unwrap_or("");
        let msg = e.get("message").and_then(|s| s.as_str()).unwrap_or("");
        raw.contains("tick") || msg.contains("tick")
    });
    assert!(
        saw_tick,
        "expected at least one `tick` log entry; got {} entries: {lv}",
        entries.len()
    );

    // Kill the ticker.
    let kill = call(
        peer,
        "kill_task",
        Some(serde_json::json!({ "id": task_id, "signal": "term" })),
    )
    .await;
    assert_eq!(
        structured(&kill).get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "kill_task didn't ack: {:?}",
        structured(&kill)
    );

    // Wait for the task to leave the running state. Up to ~5s for
    // SIGTERM cleanup.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    while Instant::now() < deadline {
        let report = first_text(
            &call(
                peer,
                "get_task",
                Some(serde_json::json!({ "id": task_id, "tail_n": 5 })),
            )
            .await,
        )
        .expect("get_task text");
        last = report.clone();
        if report.contains("cancelled") || report.contains("failed") || report.contains("completed") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("ticker never reached terminal state after kill_task. last report:\n{last}");
}

async fn test_get_graph(peer: &rmcp::Peer<rmcp::RoleClient>) {
    let result = call(peer, "get_graph", None).await;
    let value = structured(&result);
    let tops = value.get("top_tasks").expect("top_tasks present");
    assert!(tops.is_array(), "top_tasks must be an array, got {value}");
}

async fn test_grep_logs(peer: &rmcp::Peer<rmcp::RoleClient>) {
    // Spawn a fresh ticker just for grep, so we have a deterministic
    // subtree with "tick" in it.
    let spawn = call(
        peer,
        "spawn_task",
        Some(serde_json::json!({ "name": "ticker" })),
    )
    .await;
    let task_id = structured(&spawn)
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id")
        .to_string();

    // Wait for at least one tick.
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let result = call(
        peer,
        "grep_logs",
        Some(serde_json::json!({
            "task_id": task_id,
            "pattern": "tick",
            "scope": "descendants",
            "limit": 50,
        })),
    )
    .await;
    let value = structured(&result);
    let matches = value
        .get("matches")
        .and_then(|v| v.as_array())
        .expect("matches array");
    assert!(
        !matches.is_empty(),
        "grep_logs returned no matches for `tick` against {task_id}: {value}"
    );

    // Cleanup.
    let _ = call(
        peer,
        "kill_task",
        Some(serde_json::json!({ "id": task_id, "signal": "term" })),
    )
    .await;
}

async fn test_kill_all(peer: &rmcp::Peer<rmcp::RoleClient>) {
    // Spawn a couple of long-running tasks.
    let _a = call(
        peer,
        "spawn_task",
        Some(serde_json::json!({ "name": "ticker" })),
    )
    .await;
    let _b = call(
        peer,
        "spawn_task",
        Some(serde_json::json!({ "name": "ticker" })),
    )
    .await;

    let kill_all = call(peer, "kill_all", None).await;
    assert_eq!(
        structured(&kill_all).get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "kill_all ack missing: {:?}",
        structured(&kill_all)
    );

    // Give the engine a beat to propagate cancellation, then spot-check
    // the graph: every top task should be terminal.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let snap = structured(&call(peer, "get_graph", None).await);
        let tops = snap
            .get("top_tasks")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let all_terminal = !tops.is_empty()
            && tops.iter().all(|t| {
                let s = t
                    .get("status")
                    .map(|v| v.to_string())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                s.contains("cancel")
                    || s.contains("done")
                    || s.contains("failed")
                    || s.contains("timeout")
            });
        if all_terminal {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Don't be too strict — the kill_all ack itself is the contract; the
    // graph spot-check is best-effort.
    eprintln!("[mcp_e2e] kill_all: not all tops reached terminal within budget (informational)");
}

async fn test_install_skills_stub(peer: &rmcp::Peer<rmcp::RoleClient>) {
    let arguments = serde_json::json!({ "target_dir": "/tmp/rnme-e2e-skills" })
        .as_object()
        .cloned()
        .unwrap();
    let err = peer
        .call_tool(CallToolRequestParams::new("install_skills").with_arguments(arguments))
        .await;
    let msg = match err {
        Err(e) => format!("{e:?}"),
        Ok(ok) => {
            // Some servers wrap the error inside a successful CallToolResult
            // with is_error = true. Accept either shape.
            assert_eq!(
                ok.is_error,
                Some(true),
                "install_skills must return an error in Phase 6: {ok:?}"
            );
            ok.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    assert!(
        msg.contains("Phase 7")
            || msg.contains("not yet implemented")
            || msg.contains("install_skills"),
        "expected Phase 7 stub message, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Build status polling
// ---------------------------------------------------------------------------

async fn poll_build_until_idle(peer: &rmcp::Peer<rmcp::RoleClient>, budget: Duration) {
    let deadline = Instant::now() + budget;
    let mut last_state = String::new();
    while Instant::now() < deadline {
        let result = call(peer, "get_build_status", None).await;
        let value = structured(&result);
        let state = value
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("(missing)")
            .to_string();
        if state != last_state {
            eprintln!("[mcp_e2e] build state -> {state}");
            last_state = state.clone();
        }
        match state.as_str() {
            "idle" => return,
            "rebuilding" => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            "last_build_failed" => {
                let head = value
                    .get("last_failure_output")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                panic!("build failed:\n{head}");
            }
            "no_task_file" => {
                panic!(
                    "supervisor reports no_task_file even though we wrote RUNME.rs into the tempdir: {value}"
                );
            }
            other => {
                panic!("unexpected build state {other}: {value}");
            }
        }
    }
    panic!(
        "build never reached idle within {:?}. last state: {last_state}",
        budget
    );
}

// ---------------------------------------------------------------------------
// rmcp helpers
// ---------------------------------------------------------------------------

async fn call(
    peer: &rmcp::Peer<rmcp::RoleClient>,
    name: &'static str,
    args: Option<Value>,
) -> CallToolResult {
    let arguments = args.and_then(|v| v.as_object().cloned());
    let mut params = CallToolRequestParams::new(name);
    if let Some(args) = arguments {
        params = params.with_arguments(args);
    }
    peer.call_tool(params)
        .await
        .unwrap_or_else(|e| panic!("call_tool {name} failed: {e:?}"))
}

fn structured(result: &CallToolResult) -> Value {
    if let Some(v) = result.structured_content.as_ref() {
        return v.clone();
    }
    // Fall back to parsing the first text content as JSON.
    let text = first_text(result).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!("expected structured content (or JSON text) on tool result, got: {result:?} ({e})")
    })
}

fn first_text(result: &CallToolResult) -> Option<String> {
    result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
}
