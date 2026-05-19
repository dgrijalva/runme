//! Integration tests for the typed-task-invocation plan (2026-05-18).
//!
//! Fixtures are generated at test time into OS tempdirs so the live
//! `runme` binary, when run from the repo root, doesn't discover and
//! try to compile these fixtures (some of which are intentionally
//! broken). Each fixture creates its own tempdir; the tempdirs are
//! leaked (persisted past drop) so the user can `cd` to inspect them
//! manually when debugging. Tempdir paths are printed to stderr at
//! creation time — visible under `cargo test -- --nocapture` and on
//! test panic.
//!
//! Fixtures:
//!   - positive fixture — multi-level tree exercising in-file typed
//!     calls for all three arg forms, dynamic-path parity, cross-file
//!     typed calls into descendants via the auto-injected `subtasks::`
//!     tree, descendant-type construction in the parent, reachability
//!     through a structural-only intermediate dir, and `[rnme.rename]`
//!     on `services/api/RUNME.rs` and `HelloWorld/RUNME.rs`.
//!   - must-use fixture — calling fn carries `#[deny(unused_must_use)]`;
//!     a bare `worker(ctx);` call must turn the `#[must_use]` on
//!     `TaskBuilder` into a compile error.
//!   - collision fixture — negative; sibling normalization collision
//!     the build must reject.
//!   - collision-resolved fixture — same pair with one sibling renamed;
//!     must build cleanly.

mod harness;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

// =====================================================================
// Fixture generation
// =====================================================================

/// Create a fresh tempdir for a fixture with a descriptive prefix, leak
/// it so it persists past the test (the user can `cd` to inspect it
/// when debugging), and print its path to stderr.
fn make_fixture_dir(name: &str) -> PathBuf {
    let prefix = format!("rnme-fixture-{}-", name);
    let tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir()
        .expect("failed to create temp dir");
    let path = tmp.keep();
    eprintln!("fixture {name} at: {}", path.display());
    path
}

fn write_file(root: &Path, rel: &str, contents: &str) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("failed to create {}: {}", parent.display(), e));
    }
    std::fs::write(&full, contents)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", full.display(), e));
}

// ---------- typed_invocation (positive) ----------

const POSITIVE_ROOT: &str = r#"//! Root RUNME for the typed-invocation positive fixture.

use rnme::prelude::*;
use clap::Parser;

// =====================================================================
// Form-1 (zero args) callee + caller
// =====================================================================

/// Zero-args callee. Logs an identifying line so the test driver can
/// assert this task ran as a *separate* child of `caller_in_file`.
#[rnme::task]
async fn root_noop(_ctx: &TaskContext) -> TaskResult {
    info!("root_noop ran");
    Ok(())
}

/// In-file typed call to a Form-1 task. Exercises plan acceptance §3.
#[rnme::task]
async fn caller_in_file(ctx: &TaskContext) -> TaskResult {
    info!("caller_in_file: about to invoke root_noop");
    root_noop(ctx).await?;
    info!("caller_in_file: root_noop completed");
    Ok(())
}

// =====================================================================
// Form-2 (simple primitives) callee + caller
// =====================================================================

/// Simple-primitives callee. Two bool args.
#[rnme::task]
async fn primitives_callee(_ctx: &TaskContext, release: bool, verbose: bool) -> TaskResult {
    info!(
        "primitives_callee ran with release={} verbose={}",
        release, verbose
    );
    Ok(())
}

/// In-file typed call to a Form-2 task with typed positional args.
#[rnme::task]
async fn caller_in_file_form2(ctx: &TaskContext) -> TaskResult {
    info!("caller_in_file_form2: about to invoke primitives_callee");
    primitives_callee(ctx, true, false).await?;
    info!("caller_in_file_form2: primitives_callee completed");
    Ok(())
}

// =====================================================================
// Form-3 (parser struct) callee + caller
// =====================================================================

/// Args struct for the Form-3 callee. `Parser`/`Clone`/`Debug` are
/// required for the parser-struct convention.
#[derive(Parser, Clone, Debug)]
pub struct RootOpts {
    /// Target name, just to have a string arg in the struct.
    #[arg(long)]
    pub target: String,

    /// Whether to dry-run.
    #[arg(long)]
    pub dry_run: bool,
}

/// Parser-struct callee.
#[rnme::task]
async fn struct_arg_callee(_ctx: &TaskContext, opts: RootOpts) -> TaskResult {
    info!(
        "struct_arg_callee ran with target={} dry_run={}",
        opts.target, opts.dry_run
    );
    Ok(())
}

/// In-file typed call to a Form-3 task with a constructed struct.
#[rnme::task]
async fn caller_in_file_form3(ctx: &TaskContext) -> TaskResult {
    info!("caller_in_file_form3: about to invoke struct_arg_callee");
    let opts = RootOpts {
        target: "production".to_string(),
        dry_run: true,
    };
    struct_arg_callee(ctx, opts).await?;
    info!("caller_in_file_form3: struct_arg_callee completed");
    Ok(())
}

// =====================================================================
// Dynamic-path parity
// =====================================================================

/// Dynamic-path invocation resolving an in-file task by string name.
#[rnme::task]
async fn caller_dynamic(ctx: &TaskContext) -> TaskResult {
    info!("caller_dynamic: about to invoke root_noop via ctx.run");
    ctx.run("root_noop", &[]).await?;
    info!("caller_dynamic: root_noop completed via dynamic path");
    Ok(())
}

// =====================================================================
// Cross-file typed calls (Phase 3 — subtasks-injection)
// =====================================================================

/// Cross-file typed call: parent invokes a descendant task via the
/// auto-injected `subtasks` module tree.
#[rnme::task]
async fn caller_cross_file(ctx: &TaskContext) -> TaskResult {
    info!("caller_cross_file: about to invoke subtasks::services::api_v2::deploy");
    let opts = subtasks::services::api_v2::ApiDeployOpts {
        target: "production".to_string(),
        canary: false,
    };
    subtasks::services::api_v2::deploy(ctx, opts).await?;
    info!("caller_cross_file: deploy completed");
    Ok(())
}

/// Cross-file typed call constructing a descendant-defined struct.
#[rnme::task]
async fn caller_uses_child_type(ctx: &TaskContext) -> TaskResult {
    let opts = subtasks::services::api_v2::ApiDeployOpts {
        target: "staging".to_string(),
        canary: true,
    };
    info!("caller_uses_child_type: constructed opts={:?}", opts);
    subtasks::services::api_v2::deploy(ctx, opts).await?;
    info!("caller_uses_child_type: deploy completed");
    Ok(())
}

/// Cross-file typed call into a leaf whose parent dir
/// (`structural_only/`) has no RUNME.rs of its own.
#[rnme::task]
async fn caller_structural_only_leaf(ctx: &TaskContext) -> TaskResult {
    info!("caller_structural_only_leaf: about to invoke subtasks::structural_only::leaf::leaf_task");
    subtasks::structural_only::leaf::leaf_task(ctx).await?;
    info!("caller_structural_only_leaf: leaf_task completed");
    Ok(())
}
"#;

const POSITIVE_HELLOWORLD: &str = r#"//! [rnme.rename]
//! name = "Hello World"

use rnme::prelude::*;

/// Trivial task whose group key should be `hello_world`.
#[rnme::task]
async fn greet(_ctx: &TaskContext) -> TaskResult {
    info!("hello_world::greet ran");
    Ok(())
}
"#;

const POSITIVE_CHILD_A: &str = r#"//! Simple-primitives (Form-2) leaf task.

use rnme::prelude::*;

/// Build something with primitive bool args.
#[rnme::task]
async fn build(_ctx: &TaskContext, release: bool, verbose: bool) -> TaskResult {
    info!("child_a::build ran with release={} verbose={}", release, verbose);
    Ok(())
}
"#;

const POSITIVE_SERVICES: &str = r#"//! Intermediate-tier RUNME — both has its own tasks and descendants.

use rnme::prelude::*;

/// Logs an overview message. Smoke task for the intermediate tier.
#[rnme::task]
async fn services_overview(_ctx: &TaskContext) -> TaskResult {
    info!("services_overview ran");
    Ok(())
}
"#;

const POSITIVE_SERVICES_API: &str = r#"//! [rnme.rename]
//! name = "api_v2"

use rnme::prelude::*;
use clap::Parser;

/// Options for the `deploy` task.
#[derive(Parser, Clone, Debug)]
pub struct ApiDeployOpts {
    /// Deployment target (e.g. "staging", "production").
    #[arg(long)]
    pub target: String,

    /// Whether to deploy as a canary first.
    #[arg(long)]
    pub canary: bool,
}

/// Deploy the API. Form-3 task: takes a parsed struct arg.
#[rnme::task]
async fn deploy(_ctx: &TaskContext, opts: ApiDeployOpts) -> TaskResult {
    info!(
        "api_v2::deploy ran with target={} canary={}",
        opts.target, opts.canary
    );
    Ok(())
}

/// Health-check the API on a port. Form-2 task: simple primitives.
#[rnme::task]
async fn health(_ctx: &TaskContext, port: u16) -> TaskResult {
    info!("api_v2::health ran on port {}", port);
    Ok(())
}
"#;

const POSITIVE_STRUCTURAL_LEAF: &str = r#"//! Leaf below a structural-only intermediate dir.

use rnme::prelude::*;

/// Trivial task to confirm reachability under a structural-only parent.
#[rnme::task]
async fn leaf_task(_ctx: &TaskContext) -> TaskResult {
    info!("structural_only::leaf::leaf_task ran");
    Ok(())
}
"#;

fn make_positive_fixture() -> PathBuf {
    let root = make_fixture_dir("typed-invocation");
    write_file(&root, "RUNME.rs", POSITIVE_ROOT);
    write_file(&root, "HelloWorld/RUNME.rs", POSITIVE_HELLOWORLD);
    write_file(&root, "child_a/RUNME.rs", POSITIVE_CHILD_A);
    write_file(&root, "services/RUNME.rs", POSITIVE_SERVICES);
    write_file(&root, "services/api/RUNME.rs", POSITIVE_SERVICES_API);
    write_file(
        &root,
        "structural_only/leaf/RUNME.rs",
        POSITIVE_STRUCTURAL_LEAF,
    );
    root
}

// ---------- typed_invocation_must_use ----------

const MUST_USE_ROOT: &str = r#"//! Verifies that calling a task fn without `.await?` or `.spawn()?`
//! triggers the `unused_must_use` lint.

use rnme::prelude::*;

/// A trivial task. Used as the callee whose unused builder must trip
/// the lint.
#[rnme::task]
async fn worker(_ctx: &TaskContext) -> TaskResult {
    info!("worker ran");
    Ok(())
}

/// Calls `worker(ctx)` without `.await?` or `.spawn()?`.
#[deny(unused_must_use)]
#[rnme::task]
async fn bare_caller(ctx: &TaskContext) -> TaskResult {
    worker(ctx);
    Ok(())
}
"#;

fn make_must_use_fixture() -> PathBuf {
    let root = make_fixture_dir("typed-invocation-must-use");
    write_file(&root, "RUNME.rs", MUST_USE_ROOT);
    root
}

// ---------- typed_invocation_collision (negative) ----------

const COLLISION_ROOT: &str = r#"//! Root for the unresolved-collision negative fixture.

use rnme::prelude::*;

/// Trivial root task — exists so the root RUNME.rs has any task at all.
#[rnme::task]
async fn noop(_ctx: &TaskContext) -> TaskResult {
    info!("collision-root noop ran");
    Ok(())
}
"#;

const COLLISION_FOO_BAR: &str = r#"//! Sibling whose dir name `foo_bar` is already in normalized form — but
//! collides with `foo-bar/`.

use rnme::prelude::*;

/// Trivial task.
#[rnme::task]
async fn from_undered(_ctx: &TaskContext) -> TaskResult {
    info!("from_undered ran");
    Ok(())
}
"#;

const COLLISION_FOO_DASH_BAR: &str = r#"//! Sibling whose dir name `foo-bar` normalizes to `foo_bar` — collides
//! with the `foo_bar/` sibling.

use rnme::prelude::*;

/// Trivial task — body is irrelevant; this file exists to trigger the
/// sibling-normalization collision.
#[rnme::task]
async fn from_dashed(_ctx: &TaskContext) -> TaskResult {
    info!("from_dashed ran");
    Ok(())
}
"#;

fn make_collision_fixture() -> PathBuf {
    let root = make_fixture_dir("typed-invocation-collision");
    write_file(&root, "RUNME.rs", COLLISION_ROOT);
    write_file(&root, "foo_bar/RUNME.rs", COLLISION_FOO_BAR);
    write_file(&root, "foo-bar/RUNME.rs", COLLISION_FOO_DASH_BAR);
    root
}

// ---------- typed_invocation_collision_resolved ----------

const COLLISION_RESOLVED_ROOT: &str = r#"//! Root for the resolved-collision positive fixture.

use rnme::prelude::*;

/// Trivial root task.
#[rnme::task]
async fn noop(_ctx: &TaskContext) -> TaskResult {
    info!("collision-resolved-root noop ran");
    Ok(())
}
"#;

const COLLISION_RESOLVED_FOO_BAR: &str = r#"//! Sibling whose dir name `foo_bar` normalizes to `foo_bar`. No
//! rename needed — the other sibling (`foo-bar/`) was renamed.

use rnme::prelude::*;

/// Trivial task.
#[rnme::task]
async fn from_undered_resolved(_ctx: &TaskContext) -> TaskResult {
    info!("from_undered_resolved ran (group should be foo_bar)");
    Ok(())
}
"#;

const COLLISION_RESOLVED_FOO_DASH_BAR: &str = r#"//! [rnme.rename]
//! name = "foo_bar_dashed"

use rnme::prelude::*;

/// Trivial task.
#[rnme::task]
async fn from_dashed_resolved(_ctx: &TaskContext) -> TaskResult {
    info!("from_dashed_resolved ran (group should be foo_bar_dashed)");
    Ok(())
}
"#;

fn make_collision_resolved_fixture() -> PathBuf {
    let root = make_fixture_dir("typed-invocation-collision-resolved");
    write_file(&root, "RUNME.rs", COLLISION_RESOLVED_ROOT);
    write_file(&root, "foo_bar/RUNME.rs", COLLISION_RESOLVED_FOO_BAR);
    write_file(&root, "foo-bar/RUNME.rs", COLLISION_RESOLVED_FOO_DASH_BAR);
    root
}

// One tempdir per fixture, shared across the tests that target it.

static POSITIVE_FIXTURE: LazyLock<PathBuf> = LazyLock::new(make_positive_fixture);
static MUST_USE_FIXTURE: LazyLock<PathBuf> = LazyLock::new(make_must_use_fixture);
static COLLISION_FIXTURE: LazyLock<PathBuf> = LazyLock::new(make_collision_fixture);
static COLLISION_RESOLVED_FIXTURE: LazyLock<PathBuf> =
    LazyLock::new(make_collision_resolved_fixture);

// =====================================================================
// Phase 2 — typed-shim-macro tests (live)
// =====================================================================

/// Test 1: positive fixture builds and a leaf task runs.
#[test]
fn lists_all_typed_tasks() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.as_path(), &["--cli", "root_noop"]);
    out.assert_success();
    out.assert_stdout_contains("root_noop ran");
}

/// Test 2a: in-file typed call (Form-1, zero-args callee).
#[test]
fn in_file_typed_call_form1() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.as_path(), &["--cli", "caller_in_file"]);
    out.assert_success();
    out.assert_stdout_contains("caller_in_file: about to invoke root_noop");
    out.assert_stdout_contains("root_noop ran");
    out.assert_stdout_contains("caller_in_file: root_noop completed");
}

/// Test 2b: in-file typed call (Form-2, simple-primitives callee).
#[test]
fn in_file_typed_call_form2() {
    let out = harness::run_rnme(
        POSITIVE_FIXTURE.as_path(),
        &["--cli", "caller_in_file_form2"],
    );
    out.assert_success();
    out.assert_stdout_contains("caller_in_file_form2: about to invoke primitives_callee");
    out.assert_stdout_contains("primitives_callee ran with release=true verbose=false");
    out.assert_stdout_contains("caller_in_file_form2: primitives_callee completed");
}

/// Test 2c: in-file typed call (Form-3, parser-struct callee).
#[test]
fn in_file_typed_call_form3() {
    let out = harness::run_rnme(
        POSITIVE_FIXTURE.as_path(),
        &["--cli", "caller_in_file_form3"],
    );
    out.assert_success();
    out.assert_stdout_contains("caller_in_file_form3: about to invoke struct_arg_callee");
    out.assert_stdout_contains("struct_arg_callee ran with target=production dry_run=true");
    out.assert_stdout_contains("caller_in_file_form3: struct_arg_callee completed");
}

/// Test 3 (Phase 2): `unused_must_use` triggers on a bare call site.
#[test]
fn unused_must_use_warning_on_bare_call() {
    let out = harness::run_rnme(MUST_USE_FIXTURE.as_path(), &["--cli", "bare_caller"]);
    assert_ne!(
        out.exit_code, 0,
        "expected non-zero exit when #[deny(unused_must_use)] catches a bare task call; \
         got exit=0\nstdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    out.assert_stderr_contains("unused");
    out.assert_stderr_contains("must");
    out.assert_stderr_contains("worker");
}

/// Test 4: dynamic-path resolves the same task as a typed call.
#[test]
fn dynamic_path_agrees_with_typed_path() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.as_path(), &["--cli", "caller_dynamic"]);
    out.assert_success();
    out.assert_stdout_contains("caller_dynamic: about to invoke root_noop via ctx.run");
    out.assert_stdout_contains("root_noop ran");
    out.assert_stdout_contains("caller_dynamic: root_noop completed via dynamic path");
}

// =====================================================================
// Phase 2/3 — apply-rename tests (live)
// =====================================================================

/// `[rnme.rename]` propagates to the inventory group key.
#[test]
fn rename_propagates_to_group_and_module() {
    let out = harness::run_rnme(
        POSITIVE_FIXTURE.as_path(),
        &["--cli", "services/api_v2:deploy", "--target", "rename-check"],
    );
    out.assert_success();
    out.assert_stdout_contains("api_v2::deploy ran with target=rename-check");
}

/// `[rnme.rename]` runs the substituted name through heck snake-casing.
#[test]
fn rename_heck_normalizes_to_snake_case() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.as_path(), &["--cli", "hello_world:greet"]);
    out.assert_success();
    out.assert_stdout_contains("hello_world::greet ran");
}

/// Rename resolves what would otherwise be a sibling collision.
#[test]
fn rename_resolves_collision() {
    let dashed = harness::run_rnme(
        COLLISION_RESOLVED_FIXTURE.as_path(),
        &["--cli", "foo_bar_dashed:from_dashed_resolved"],
    );
    dashed.assert_success();
    dashed.assert_stdout_contains("from_dashed_resolved ran");

    let undered = harness::run_rnme(
        COLLISION_RESOLVED_FIXTURE.as_path(),
        &["--cli", "foo_bar:from_undered_resolved"],
    );
    undered.assert_success();
    undered.assert_stdout_contains("from_undered_resolved ran");
}

// =====================================================================
// Phase 3 — cross-file typed call tests (live)
// =====================================================================

/// Cross-file typed call invokes a descendant.
#[test]
fn cross_file_typed_call_runs_descendant() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.as_path(), &["--cli", "caller_cross_file"]);
    out.assert_success();
    out.assert_stdout_contains(
        "caller_cross_file: about to invoke subtasks::services::api_v2::deploy",
    );
    out.assert_stdout_contains("api_v2::deploy ran with target=production canary=false");
    out.assert_stdout_contains("caller_cross_file: deploy completed");
}

/// Descendant types are constructible from a parent.
#[test]
fn descendant_type_constructible_from_parent() {
    let out = harness::run_rnme(
        POSITIVE_FIXTURE.as_path(),
        &["--cli", "caller_uses_child_type"],
    );
    out.assert_success();
    out.assert_stdout_contains("caller_uses_child_type: constructed opts=");
    out.assert_stdout_contains("api_v2::deploy ran with target=staging canary=true");
    out.assert_stdout_contains("caller_uses_child_type: deploy completed");
}

/// Intermediate-tier and structural-only dirs don't break descendant
/// paths.
#[test]
fn intermediate_runme_does_not_break_descendants() {
    let api_out = harness::run_rnme(
        POSITIVE_FIXTURE.as_path(),
        &["--cli", "services/api_v2:deploy", "--target", "staging"],
    );
    api_out.assert_success();
    api_out.assert_stdout_contains("api_v2::deploy ran with target=staging");

    let leaf_out = harness::run_rnme(
        POSITIVE_FIXTURE.as_path(),
        &["--cli", "caller_structural_only_leaf"],
    );
    leaf_out.assert_success();
    leaf_out.assert_stdout_contains(
        "caller_structural_only_leaf: about to invoke \
         subtasks::structural_only::leaf::leaf_task",
    );
    leaf_out.assert_stdout_contains("structural_only::leaf::leaf_task ran");
}

// =====================================================================
// Phase 4 — sibling-collision negative case (live)
// =====================================================================

/// Unresolved sibling collision is rejected at workspace generation
/// (negative).
#[test]
fn rename_collision_is_rejected() {
    let out = harness::run_rnme(COLLISION_FIXTURE.as_path(), &["--cli", "noop"]);
    assert_ne!(
        out.exit_code, 0,
        "expected non-zero exit for unresolved collision; got stdout={} stderr={}",
        out.stdout, out.stderr
    );
    out.assert_stderr_contains("foo-bar");
    out.assert_stderr_contains("foo_bar");
    out.assert_stderr_contains("[rnme.rename]");
    out.assert_stderr_contains("foo_bar_dashed");
}
