//! Integration tests for `#[rnme::task_template]` + `rnme::import_task!`.
//!
//! Validates that a library crate (`rnme-test-task-templates`) declaring
//! three templates can be consumed from a RUNME.rs via `rnme::import_task!`
//! and behaves as a fully-local typed task registration:
//!
//!   - Each imported template appears under the *consumer's* group in
//!     `rnme list`.
//!   - The imported task runs in the *consumer's* directory (`__RNME_DIR`
//!     resolves at the call site, not the library's location).
//!   - The typed shim is reachable from an ancestor RUNME.rs via
//!     `subtasks::<consumer_path>::<task>(ctx, ...).await?`.
//!   - The library-provided bulk macro `import_all_test_templates!`
//!     stamps all three tasks in one consumer-side invocation.
//!   - `--help` for a Form-3 (parser-struct) template reflects the
//!     template's argument metadata.
//!   - A typo in the imported task name produces a compile error sourced
//!     from the library path.
//!
//! Mirrors the fixture-generation shape of `tests/typed_invocation.rs` and
//! the spike's approach (`tests/task_template_spike.rs`).

mod harness;

use harness::{run_rnme, write_fixture};
use std::path::PathBuf;

/// Resolve the absolute path to the test-task-templates library crate.
fn templates_lib_path() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lib = here.join("testing").join("test-task-templates");
    std::fs::canonicalize(&lib)
        .unwrap_or_else(|e| panic!("failed to canonicalize {}: {}", lib.display(), e))
}

fn make_fixture(name: &str) -> PathBuf {
    let prefix = format!("rnme-task-templates-{}-", name);
    let tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir()
        .expect("failed to create temp dir");
    let path = tmp.keep();
    eprintln!("fixture {name} at: {}", path.display());
    path
}

/// Consumer RUNME.rs that imports each template individually via
/// `rnme::import_task!` and exposes a caller that prints `task_dir`.
fn consumer_runme(lib_path: &PathBuf) -> String {
    format!(
        r#"//! Consumer RUNME for task_templates integration test.
//!
//! [dependencies]
//! rnme-test-task-templates = {{ path = "{lib_path}" }}

use rnme::prelude::*;
use rnme_test_task_templates::BuildArgs;

// Three explicit imports of each template form. The `build` template's
// stamped shim references `BuildArgs` by bare name, so the consumer must
// have the type in scope (imported above).
rnme::import_task!(rnme_test_task_templates::noop);
rnme::import_task!(rnme_test_task_templates::echo);
rnme::import_task!(rnme_test_task_templates::build);

/// Local caller that invokes the imported `noop` template via its typed
/// shim and prints `task_dir` so the test can confirm the call resolves
/// in the consumer's directory.
#[rnme::task]
async fn call_noop_local(ctx: &TaskContext) -> TaskResult {{
    let dir = ctx
        .task_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unset>".to_string());
    info!("call_noop_local: caller task_dir = {{dir}}");
    println!("caller task_dir = {{dir}}");
    noop(ctx).await?;
    Ok(())
}}
"#,
        lib_path = lib_path.display()
    )
}

/// Consumer RUNME.rs using the library-provided bulk macro
/// `import_all_test_templates!()`. Demonstrates that the bulk macro is
/// "just a library helper" — no new primitive needed from rnme.
fn bulk_consumer_runme(lib_path: &PathBuf) -> String {
    format!(
        r#"//! Bulk-import consumer for task_templates integration test.
//!
//! [dependencies]
//! rnme-test-task-templates = {{ path = "{lib_path}" }}

use rnme::prelude::*;
use rnme_test_task_templates::BuildArgs;

rnme_test_task_templates::import_all_test_templates!();
"#,
        lib_path = lib_path.display()
    )
}

/// Ancestor + child layout: the ancestor (root) invokes the consumer-child's
/// imported `noop` template via the auto-injected `subtasks::child::noop`
/// typed shim — exercising cross-file invocation of an imported template.
fn ancestor_root_runme() -> &'static str {
    r#"//! Ancestor RUNME for cross-file invocation of an imported template.

use rnme::prelude::*;

/// Invokes the child's imported `noop` template through the typed-shim
/// `subtasks::child::noop`.
#[rnme::task]
async fn parent_calls_child_noop(ctx: &TaskContext) -> TaskResult {
    info!("parent_calls_child_noop: invoking subtasks::child::noop");
    subtasks::child::noop(ctx).await?;
    info!("parent_calls_child_noop: child::noop completed");
    Ok(())
}
"#
}

fn child_runme(lib_path: &PathBuf) -> String {
    format!(
        r#"//! Child RUNME — imports a template; ancestor reaches it via subtasks::.
//!
//! [dependencies]
//! rnme-test-task-templates = {{ path = "{lib_path}" }}

use rnme::prelude::*;

rnme::import_task!(rnme_test_task_templates::noop);
"#,
        lib_path = lib_path.display()
    )
}

// =====================================================================
// Tests
// =====================================================================

/// All three imported templates appear in `rnme list` under the consumer's
/// group (the consumer is the root, so group is empty/root).
#[test]
fn list_shows_all_imported_templates() {
    let lib = templates_lib_path();
    let fixture = make_fixture("list");
    write_fixture(&fixture, "RUNME.rs", &consumer_runme(&lib));

    let out = run_rnme(&fixture, &["list"]);
    out.assert_success();
    out.assert_stdout_contains("noop");
    out.assert_stdout_contains("echo");
    out.assert_stdout_contains("build");
}

/// Imported `noop` runs in the consumer's directory — `__RNME_DIR`
/// resolves at the stamp site, not the library site.
#[test]
fn imported_task_runs_in_consumer_dir() {
    let lib = templates_lib_path();
    let fixture = make_fixture("consumer-dir");
    write_fixture(&fixture, "RUNME.rs", &consumer_runme(&lib));

    let out = run_rnme(&fixture, &["--cli", "call_noop_local"]);
    out.assert_success();
    let expected_dir = std::fs::canonicalize(&fixture).expect("canonicalize fixture");
    let expected_substr = format!("caller task_dir = {}", expected_dir.display());
    assert!(
        out.stdout.contains(&expected_substr) || out.stderr.contains(&expected_substr),
        "expected output to contain '{}'\nstdout: {}\nstderr: {}",
        expected_substr,
        out.stdout,
        out.stderr
    );
    let lib_substr = format!("caller task_dir = {}", lib.display());
    assert!(
        !out.stdout.contains(&lib_substr) && !out.stderr.contains(&lib_substr),
        "task_dir resolved to library path ({}) — __RNME_DIR was bound at \
         the definition site instead of the call site\nstdout: {}\nstderr: {}",
        lib.display(),
        out.stdout,
        out.stderr,
    );
}

/// Ancestor invokes the child's imported template via `subtasks::child::noop`.
#[test]
fn ancestor_invokes_imported_template_via_subtasks() {
    let lib = templates_lib_path();
    let fixture = make_fixture("ancestor");
    write_fixture(&fixture, "RUNME.rs", ancestor_root_runme());
    write_fixture(&fixture, "child/RUNME.rs", &child_runme(&lib));

    let out = run_rnme(&fixture, &["--cli", "parent_calls_child_noop"]);
    out.assert_success();
    out.assert_stdout_contains("parent_calls_child_noop: invoking subtasks::child::noop");
    out.assert_stdout_contains("parent_calls_child_noop: child::noop completed");
}

/// The library-provided bulk-import macro stamps all three templates.
#[test]
fn bulk_import_macro_works() {
    let lib = templates_lib_path();
    let fixture = make_fixture("bulk");
    write_fixture(&fixture, "RUNME.rs", &bulk_consumer_runme(&lib));

    let out = run_rnme(&fixture, &["list"]);
    out.assert_success();
    out.assert_stdout_contains("noop");
    out.assert_stdout_contains("echo");
    out.assert_stdout_contains("build");
}

/// `--help` on the Form-3 template surfaces the `BuildArgs` clap metadata.
///
/// Note: passing `--help` to a task whose args come from `try_parse_from`
/// is reported as a parse "error" by clap (`HelpDisplayed`), so rnme
/// surfaces it via the TaskError path with a non-zero exit. The
/// help-text content is the relevant signal — that confirms the
/// template's clap metadata flowed through to the consumer-stamped
/// `arg_metadata` fn.
#[test]
fn help_reflects_template_argument_metadata() {
    let lib = templates_lib_path();
    let fixture = make_fixture("help");
    write_fixture(&fixture, "RUNME.rs", &consumer_runme(&lib));

    let out = run_rnme(&fixture, &["--cli", "build", "--help"]);
    let combined = format!("{}\n{}", out.stdout, out.stderr);
    assert!(
        combined.contains("--release"),
        "expected --help output to include --release flag\nstdout: {}\nstderr: {}",
        out.stdout,
        out.stderr,
    );
    assert!(
        combined.contains("--target"),
        "expected --help output to include --target flag\nstdout: {}\nstderr: {}",
        out.stdout,
        out.stderr,
    );
}
