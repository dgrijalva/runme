//! Spike test for the `task_template` design (plan T1).
//!
//! Validates the central design assumption of `docs/task_templates.md`:
//! a `#[macro_export] macro_rules!` defined in a library crate, invoked
//! at a consumer's RUNME.rs site, can stamp out a fully-local typed
//! task registration whose `TaskDef::group` and `TaskDef::dir` resolve
//! to the *consumer's* `__RNME_GROUP` / `__RNME_DIR`, while the
//! underlying body / wrapper / argmeta functions live in the library
//! (and are reached via `$crate::...` from inside the macro_rules
//! expansion).
//!
//! No proc macros are involved on the library side. The library
//! (`rnme-test-task-template-spike`) hand-writes exactly the token
//! shape that `#[rnme::task_template]` will eventually emit.
//!
//! What this test verifies (plan T1 acceptance):
//!
//! - `cargo build` of the workspace succeeds (implicit — this test
//!   wouldn't link otherwise).
//! - `rnme list` against a consumer RUNME.rs that calls
//!   `rnme_test_task_template_spike::__rnme_stamp_demo!();` shows the
//!   `demo` task under the consumer's group.
//! - Running the imported task runs it in the consumer's directory
//!   (the body prints `task_dir = <path>`; we assert that path is the
//!   consumer's tempdir, not the library crate's location).
//! - The hand-rolled `macro_rules!` expansion compiles when invoked at
//!   the consumer site — i.e. `__RNME_GROUP` / `__RNME_DIR` resolve at
//!   the call site, and `$crate::__runme_taskfn_demo` /
//!   `$crate::__rnme_body_demo` / `$crate::__runme_argmeta_demo`
//!   resolve at the library site, *in the same expansion*.

mod harness;

use harness::{run_rnme, write_fixture};
use std::path::PathBuf;

/// Resolve the absolute path to the spike library crate so the consumer
/// RUNME.rs can declare a `path = "..."` dependency on it.
///
/// Uses `CARGO_MANIFEST_DIR` (set by cargo for integration tests) to
/// reach the spike crate's manifest. Canonicalizes so the path remains
/// valid when the consumer tempdir lives somewhere else on disk.
fn spike_lib_path() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lib = here.join("testing").join("test-task-template-spike");
    std::fs::canonicalize(&lib)
        .unwrap_or_else(|e| panic!("failed to canonicalize {}: {}", lib.display(), e))
}

/// Create a fresh tempdir for the spike fixture. Leaked so the user
/// can `cd` to inspect it on test failure.
fn make_fixture() -> PathBuf {
    let tmp = tempfile::Builder::new()
        .prefix("rnme-spike-task-template-")
        .tempdir()
        .expect("failed to create temp dir");
    let path = tmp.keep();
    eprintln!("spike fixture at: {}", path.display());
    path
}

/// Generate the consumer RUNME.rs that imports the spike library and
/// stamps the `demo` template into a local task.
fn consumer_runme(lib_path: &PathBuf) -> String {
    // The frontmatter declares a path dep on the spike crate. The crate
    // re-exports its macro_rules! at its crate root because the macro
    // is `#[macro_export]`.
    format!(
        r#"//! Spike consumer RUNME.
//!
//! [dependencies]
//! rnme-test-task-template-spike = {{ path = "{lib_path}" }}

use rnme::prelude::*;

// A normal `#[rnme::task]` defined locally in this consumer file, to
// confirm imported and local tasks coexist.
/// A locally-defined task for comparison.
#[rnme::task]
async fn local_only(_ctx: &TaskContext) -> TaskResult {{
    info!("local_only ran");
    Ok(())
}}

// Stamp the library's template into a local task registration. After
// expansion this consumer crate has:
//
//   - pub static __RNME_TASKDEF_demo: TaskDef {{ group: __RNME_GROUP,
//     dir: __RNME_DIR, func: TaskFnKind::Static(<library>::__runme_taskfn_demo), ... }}
//   - inventory::submit!(TaskDefRef(&__RNME_TASKDEF_demo))
//   - pub fn demo(ctx) -> TaskBuilder
//
// All three reference the consumer's __RNME_GROUP / __RNME_DIR (injected
// by codegen) and the library's body/wrapper fns (via $crate::...).
rnme_test_task_template_spike::__rnme_stamp_demo!();
"#,
        lib_path = lib_path.display()
    )
}

// =====================================================================
// Spike acceptance test
// =====================================================================

/// Acceptance: the consumer's RUNME.rs builds, `rnme list` shows the
/// stamped `demo` task, running it executes in the consumer's
/// directory (proving __RNME_DIR resolved at the call site).
#[test]
fn spike_stamp_macro_runs_in_consumer_dir() {
    let lib = spike_lib_path();
    let fixture = make_fixture();
    write_fixture(&fixture, "RUNME.rs", &consumer_runme(&lib));

    // --- `rnme list` ---
    let list = run_rnme(&fixture, &["list"]);
    list.assert_success();
    // Both the stamped-in template task and the locally-defined task
    // should appear.
    list.assert_stdout_contains("demo");
    list.assert_stdout_contains("local_only");
    // The description text is encoded inside the library's
    // __rnme_stamp_demo! arm; if the stamp produced the right TaskDef
    // then `rnme list` will surface it. Confirm to rule out "task with
    // garbage metadata" failure modes.
    list.assert_stdout_contains("Demo task template");

    // --- run the stamped task (rnme invokes tasks directly: `rnme demo`, no `run` subcommand) ---
    let run = run_rnme(&fixture, &["--cli", "demo"]);
    run.assert_success();
    // The body prints `demo task_dir = <path>`. The plan's central
    // claim is that __RNME_DIR resolves at the consumer site — i.e. to
    // the directory containing the consumer's RUNME.rs, *not* the
    // library crate's directory.
    let expected_dir = std::fs::canonicalize(&fixture).expect("canonicalize fixture");
    let expected_substr = format!("demo task_dir = {}", expected_dir.display());
    assert!(
        run.stdout.contains(&expected_substr) || run.stderr.contains(&expected_substr),
        "expected output to contain '{}'\nstdout: {}\nstderr: {}",
        expected_substr,
        run.stdout,
        run.stderr
    );
    // Sanity: the library crate's own path should NOT appear as the
    // task_dir — that would indicate __RNME_DIR resolved at the
    // definition site instead of the call site.
    let lib_substr = format!("demo task_dir = {}", lib.display());
    assert!(
        !run.stdout.contains(&lib_substr) && !run.stderr.contains(&lib_substr),
        "task_dir resolved to library path ({}) — __RNME_DIR was bound at \
         the definition site instead of the call site\nstdout: {}\nstderr: {}",
        lib.display(),
        run.stdout,
        run.stderr,
    );
}
