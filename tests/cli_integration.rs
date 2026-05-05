//! CLI subprocess integration tests.
//!
//! Each test creates a temp directory, writes RUNME.rs fixtures, and runs the
//! actual `rnme` binary against them. Tests are grouped to reuse compiled
//! fixtures where possible, but each compilation still takes several seconds.
//!
//! Run with: `cargo test --test cli_integration`
//! Or to include ignored (slow) tests: `cargo test --test cli_integration -- --ignored`

mod harness;

use std::sync::LazyLock;
use tempfile::TempDir;

/// Shared temp dir with the kitchen-sink fixture pre-written.
/// The first test to access this triggers fixture creation (but not compilation —
/// that happens when `run_rnme` is called). Subsequent tests reuse the same dir,
/// so Cargo's incremental compilation makes them faster.
static KITCHEN_SINK: LazyLock<TempDir> = LazyLock::new(|| {
    let dir = TempDir::new().expect("failed to create temp dir");
    harness::write_fixture(dir.path(), "RUNME.rs", harness::kitchen_sink_runme());
    dir
});

// ---------------------------------------------------------------------------
// Priority 1: Agent mode JSON output
//
// These tests target the internal `UiMode::Agent` (structured JSON output for
// machine consumption). The `--ui` flag has been removed and `--mcp` has not
// landed yet, so agent mode is unreachable from the CLI right now. Re-enable
// these once the MCP entry point exists.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "agent mode flag removed; will be re-enabled when --mcp lands"]
fn agent_json_success() {
    let out = harness::run_rnme(
        KITCHEN_SINK.path(),
        &["--format", "json", "succeed"],
    );
    out.assert_success();
    out.assert_json_ok("succeed");
}

#[test]
#[ignore = "agent mode flag removed; will be re-enabled when --mcp lands"]
fn agent_json_failure() {
    let out = harness::run_rnme(
        KITCHEN_SINK.path(),
        &["--format", "json", "fail_default"],
    );
    // fail_default returns Err("default failure".into()) → exit code 1
    assert_ne!(out.exit_code, 0, "expected non-zero exit for failing task");
    out.assert_json_error("fail_default");
    // The error output should contain the failure message
    let json = out.json();
    let error = json.get("error").expect("JSON should have 'error' field");
    let msg = error
        .get("message")
        .and_then(|v| v.as_str())
        .expect("error should have a 'message' field");
    assert!(
        msg.contains("default failure"),
        "error message should contain 'default failure', got: {}",
        msg
    );
}

#[test]
#[ignore = "agent mode flag removed; will be re-enabled when --mcp lands"]
fn agent_json_specific_exit_code() {
    let out = harness::run_rnme(
        KITCHEN_SINK.path(),
        &["--format", "json", "fail_with_code"],
    );
    out.assert_exit_code(42);
    out.assert_json_error("fail_with_code");
}

// ---------------------------------------------------------------------------
// Priority 2: CLI mode
// ---------------------------------------------------------------------------

#[test]
fn cli_mode_output() {
    let out = harness::run_rnme(KITCHEN_SINK.path(), &["--cli", "produce_output"]);
    out.assert_success();
    out.assert_stdout_contains("hello-from-produce-output");
}

#[test]
fn cli_mode_failure() {
    let out = harness::run_rnme(KITCHEN_SINK.path(), &["--cli", "fail_default"]);
    assert_ne!(out.exit_code, 0, "expected non-zero exit for failing task");
    out.assert_stderr_contains("Error:");
}

#[test]
fn ui_flags_conflict() {
    let out = harness::run_rnme(KITCHEN_SINK.path(), &["--tui", "--cli", "succeed"]);
    assert_ne!(
        out.exit_code, 0,
        "expected non-zero exit when both --tui and --cli are passed"
    );
}

// ---------------------------------------------------------------------------
// Priority 3: Discovery and resolution
// ---------------------------------------------------------------------------

#[test]
fn no_runme_found() {
    let empty = TempDir::new().expect("failed to create temp dir");
    let out = harness::run_rnme(empty.path(), &["--cli", "anything"]);
    assert_ne!(out.exit_code, 0, "expected non-zero exit when no RUNME.rs");
    out.assert_stderr_contains("no RUNME.rs found");
}

#[test]
fn task_not_found() {
    let out = harness::run_rnme(
        KITCHEN_SINK.path(),
        &["--cli", "nonexistent_task_xyz"],
    );
    assert_ne!(out.exit_code, 0, "expected non-zero exit for unknown task");
    out.assert_stderr_contains("unknown task");
}

#[test]
fn nested_group_resolution() {
    let dir = TempDir::new().expect("failed to create temp dir");
    // Root RUNME.rs — needed for discovery to work
    harness::write_fixture(dir.path(), "RUNME.rs", harness::minimal_runme());
    // Sub-directory RUNME.rs in "services/"
    harness::write_fixture(dir.path(), "services/RUNME.rs", harness::sub_dir_runme());

    // The sub task should be resolvable as "services:sub_task"
    let out = harness::run_rnme(dir.path(), &["--cli", "services:sub_task"]);
    out.assert_success();
}

// ---------------------------------------------------------------------------
// Priority 4: Argument forwarding
// ---------------------------------------------------------------------------

#[test]
fn arguments_forwarded_to_task() {
    // `echo_args` declares `message: String` as a required positional/option
    // arg. A successful exit proves clap parsed `--message hello-world` from
    // the forwarded task args; the task body does nothing observable beyond
    // an `info!` (which is order-racy under the engine's log forwarding).
    let out = harness::run_rnme(
        KITCHEN_SINK.path(),
        &["--cli", "echo_args", "--message", "hello-world"],
    );
    out.assert_success();
}
