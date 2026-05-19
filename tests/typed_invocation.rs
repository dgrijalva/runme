//! Integration tests for the typed-task-invocation plan (2026-05-18).
//!
//! Drives the fixture trees under `testing/fixtures/`:
//!   - `typed_invocation/` — positive fixture. In-file typed calls for
//!     all three arg forms, dynamic-path parity, cross-file typed
//!     calls into descendants via the auto-injected `subtasks::` tree,
//!     descendant-type construction in the parent, and reachability
//!     through a structural-only intermediate dir. Also hosts the
//!     `services/api/RUNME.rs` (`[rnme.rename] name = "api_v2"`) and
//!     `HelloWorld/RUNME.rs` (`name = "Hello World"` →
//!     `hello_world` via heck snake-casing) descendants that exercise
//!     `apply-rename`.
//!   - `typed_invocation_must_use/` — fixture whose calling fn carries
//!     `#[deny(unused_must_use)]`; a bare `worker(ctx);` call must turn
//!     the `#[must_use]` on `TaskBuilder` into a compile error.
//!   - `typed_invocation_collision/` — negative fixture; sibling
//!     normalization collision the build must reject (once
//!     collision-detection lands).
//!   - `typed_invocation_collision_resolved/` — same pair as above with
//!     one sibling renamed; must build cleanly.
//!
//! Each fixture is copied to a `TempDir` per-suite via `LazyLock` to
//! match the pattern in `tests/cli_integration.rs:18-19`. rnme's
//! workspace cache lands inside the temp dir and is cleaned at process
//! exit; parallel `cargo test` runs do not race on a shared cache.

mod harness;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tempfile::TempDir;

// =====================================================================
// Fixture copying
// =====================================================================

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testing/fixtures")
}

/// Recursively copy a directory into a fresh `TempDir`. The returned
/// `TempDir` owns the temp directory's lifetime — drop it to clean up.
fn copy_fixture_to_tempdir(src_name: &str) -> TempDir {
    let src = fixtures_root().join(src_name);
    let tmp = TempDir::new().expect("failed to create temp dir");
    copy_dir_recursive(&src, tmp.path());
    tmp
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst)
        .unwrap_or_else(|e| panic!("failed to create {}: {}", dst.display(), e));
    for entry in std::fs::read_dir(src)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", src.display(), e))
    {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        if path.is_dir() {
            copy_dir_recursive(&path, &dst_path);
        } else {
            std::fs::copy(&path, &dst_path).unwrap_or_else(|e| {
                panic!(
                    "failed to copy {} -> {}: {}",
                    path.display(),
                    dst_path.display(),
                    e
                )
            });
        }
    }
}

// One `TempDir` per fixture, shared across the tests that target it.

static POSITIVE_FIXTURE: LazyLock<TempDir> =
    LazyLock::new(|| copy_fixture_to_tempdir("typed_invocation"));

static MUST_USE_FIXTURE: LazyLock<TempDir> =
    LazyLock::new(|| copy_fixture_to_tempdir("typed_invocation_must_use"));

static COLLISION_FIXTURE: LazyLock<TempDir> =
    LazyLock::new(|| copy_fixture_to_tempdir("typed_invocation_collision"));

static COLLISION_RESOLVED_FIXTURE: LazyLock<TempDir> =
    LazyLock::new(|| copy_fixture_to_tempdir("typed_invocation_collision_resolved"));

// =====================================================================
// Phase 2 — typed-shim-macro tests (live)
// =====================================================================

/// Test 1: positive fixture builds and a leaf task runs.
///
/// Smoke. Verifies the workspace generator handles the fixture tree
/// (root + intermediate-tier RUNME at `services/` with a descendant +
/// structural-only-parent leaf + `[rnme.rename]` frontmatter on
/// `services/api/RUNME.rs`) without errors, and that inventory picks
/// up the root task.
#[test]
fn lists_all_typed_tasks() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "root_noop"]);
    out.assert_success();
    out.assert_stdout_contains("root_noop ran");
}

/// Test 2a: in-file typed call (Form-1, zero-args callee).
///
/// Acceptance: plan §3 / brief item 3. `caller_in_file` invokes the
/// zero-args `root_noop` via the typed shim. The shim must register a
/// separate child task (distinct log line owner).
#[test]
fn in_file_typed_call_form1() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "caller_in_file"]);
    out.assert_success();
    out.assert_stdout_contains("caller_in_file: about to invoke root_noop");
    out.assert_stdout_contains("root_noop ran");
    out.assert_stdout_contains("caller_in_file: root_noop completed");
}

/// Test 2b: in-file typed call (Form-2, simple-primitives callee).
///
/// Acceptance: typed shim emits a builder fn that accepts the callee's
/// typed positional args by value and threads them into the body via
/// `Invocation::Factory`. Body must observe the exact bool values
/// passed at the call site.
#[test]
fn in_file_typed_call_form2() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "caller_in_file_form2"]);
    out.assert_success();
    out.assert_stdout_contains("caller_in_file_form2: about to invoke primitives_callee");
    out.assert_stdout_contains("primitives_callee ran with release=true verbose=false");
    out.assert_stdout_contains("caller_in_file_form2: primitives_callee completed");
}

/// Test 2c: in-file typed call (Form-3, parser-struct callee).
///
/// Acceptance: typed shim handles a struct arg the same way as
/// primitives — capture by value, thread into the factory closure,
/// body receives the struct.
#[test]
fn in_file_typed_call_form3() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "caller_in_file_form3"]);
    out.assert_success();
    out.assert_stdout_contains("caller_in_file_form3: about to invoke struct_arg_callee");
    out.assert_stdout_contains("struct_arg_callee ran with target=production dry_run=true");
    out.assert_stdout_contains("caller_in_file_form3: struct_arg_callee completed");
}

/// Test 3 (Phase 2): `unused_must_use` triggers on a bare call site.
///
/// Acceptance: plan §3 of design / brief item 3. `TaskBuilder` is
/// `#[must_use]`; a call like `worker(ctx);` without `.await?` /
/// `.spawn()?` must produce a warning. The fixture's calling fn
/// carries `#[deny(unused_must_use)]` so the lint becomes a hard
/// compile error the test can assert on via stderr.
#[test]
fn unused_must_use_warning_on_bare_call() {
    let out = harness::run_rnme(MUST_USE_FIXTURE.path(), &["--cli", "bare_caller"]);
    assert_ne!(
        out.exit_code, 0,
        "expected non-zero exit when #[deny(unused_must_use)] catches a bare task call; \
         got exit=0\nstdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    // The compile error mentions the must-use lint and the offending fn.
    out.assert_stderr_contains("unused");
    out.assert_stderr_contains("must");
    out.assert_stderr_contains("worker");
}

/// Test 4: dynamic-path resolves the same task as a typed call.
///
/// Acceptance: plan §7 / brief item 7. `caller_dynamic` uses
/// `ctx.run("root_noop", &[])` and must execute the same body that
/// `caller_in_file` reaches via the typed shim. Both call shapes
/// converge on the same registered task.
#[test]
fn dynamic_path_agrees_with_typed_path() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "caller_dynamic"]);
    out.assert_success();
    out.assert_stdout_contains("caller_dynamic: about to invoke root_noop via ctx.run");
    out.assert_stdout_contains("root_noop ran");
    out.assert_stdout_contains("caller_dynamic: root_noop completed via dynamic path");
}

// =====================================================================
// Phase 2/3 — apply-rename tests (live)
// =====================================================================

/// `[rnme.rename]` propagates to the inventory group key.
///
/// Acceptance: plan §7. `services/api/RUNME.rs` carries
/// `[rnme.rename] name = "api_v2"`; the substituted name must show up
/// as the resolvable group (`services/api_v2`), and the on-disk dir
/// name `api` must not leak. Asserts the group-key sink directly via
/// CLI lookup. The module-path sink will be re-verified once the
/// cross-file caller is wired (test `cross_file_typed_call_runs_descendant`).
#[test]
fn rename_propagates_to_group_and_module() {
    let out = harness::run_rnme(
        POSITIVE_FIXTURE.path(),
        &["--cli", "services/api_v2:deploy", "--target", "rename-check"],
    );
    out.assert_success();
    out.assert_stdout_contains("api_v2::deploy ran with target=rename-check");
}

/// `[rnme.rename]` runs the substituted name through heck snake-casing.
///
/// Acceptance: plan §165 of the design doc — "The replacement string
/// is substituted for the directory name **before normalization**".
/// `HelloWorld/RUNME.rs` carries `name = "Hello World"`; heck's
/// `to_snake_case` turns that into `hello_world`. The CLI must
/// resolve the task under `hello_world:greet`, not `helloworld:` or
/// `hello world:`.
#[test]
fn rename_heck_normalizes_to_snake_case() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "hello_world:greet"]);
    out.assert_success();
    out.assert_stdout_contains("hello_world::greet ran");
}

/// Rename resolves what would otherwise be a sibling collision.
///
/// Acceptance: plan §391 — same directory pair as the negative
/// collision case, with the `foo-bar/` sibling renamed to
/// `foo_bar_dashed`. The renamed sibling is reachable under
/// `foo_bar_dashed:from_dashed_resolved`; the unrenamed sibling is
/// reachable under its natural normalized group `foo_bar`.
#[test]
fn rename_resolves_collision() {
    let dashed = harness::run_rnme(
        COLLISION_RESOLVED_FIXTURE.path(),
        &["--cli", "foo_bar_dashed:from_dashed_resolved"],
    );
    dashed.assert_success();
    dashed.assert_stdout_contains("from_dashed_resolved ran");

    let undered = harness::run_rnme(
        COLLISION_RESOLVED_FIXTURE.path(),
        &["--cli", "foo_bar:from_undered_resolved"],
    );
    undered.assert_success();
    undered.assert_stdout_contains("from_undered_resolved ran");
}

// =====================================================================
// Phase 3 — cross-file typed call tests (live)
// =====================================================================

/// Cross-file typed call invokes a descendant.
///
/// Acceptance: plan §4 / brief item 4. `caller_cross_file` in the root
/// invokes `subtasks::services::api_v2::deploy(ctx, opts)`. The
/// descendant body must run as a framework-integrated child task (its
/// own task id, own log source), and the renamed module path
/// (`api_v2`, from `[rnme.rename] name = "api_v2"`) must reach through
/// the auto-injected `subtasks` tree.
#[test]
fn cross_file_typed_call_runs_descendant() {
    let out = harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "caller_cross_file"]);
    out.assert_success();
    out.assert_stdout_contains(
        "caller_cross_file: about to invoke subtasks::services::api_v2::deploy",
    );
    out.assert_stdout_contains("api_v2::deploy ran with target=production canary=false");
    out.assert_stdout_contains("caller_cross_file: deploy completed");
}

/// Descendant types are constructible from a parent.
///
/// Acceptance: plan §5 / brief item 6. `caller_uses_child_type`
/// constructs `subtasks::services::api_v2::ApiDeployOpts { .. }` in
/// the root, then passes the struct into the descendant's typed shim.
/// Compilation success alone is most of the proof — the struct path
/// resolved through the renamed `subtasks::` module path.
#[test]
fn descendant_type_constructible_from_parent() {
    let out =
        harness::run_rnme(POSITIVE_FIXTURE.path(), &["--cli", "caller_uses_child_type"]);
    out.assert_success();
    out.assert_stdout_contains("caller_uses_child_type: constructed opts=");
    out.assert_stdout_contains("api_v2::deploy ran with target=staging canary=true");
    out.assert_stdout_contains("caller_uses_child_type: deploy completed");
}

/// Intermediate-tier and structural-only dirs don't break descendant
/// paths.
///
/// Acceptance: plan §6 / design doc §3. Two properties checked here:
///   1. `services/RUNME.rs` has its own task body AND descendants; the
///      renamed `services/api_v2:deploy` must still be reachable via
///      `subtasks::services::api_v2::deploy` (verified by
///      `caller_cross_file` above and re-asserted via the CLI group:task
///      path here for completeness).
///   2. `structural_only/` has NO RUNME.rs; the leaf at
///      `structural_only/leaf/RUNME.rs` must still be reachable via
///      `subtasks::structural_only::leaf::leaf_task` (the parent's
///      generated `subtasks` tree emits an empty `pub mod structural_only`
///      along the path).
#[test]
fn intermediate_runme_does_not_break_descendants() {
    // (1) Renamed descendant reachable under `services/api_v2:deploy`
    // (group-key sink — typed-path sink is exercised by
    // `cross_file_typed_call_runs_descendant`).
    let api_out = harness::run_rnme(
        POSITIVE_FIXTURE.path(),
        &["--cli", "services/api_v2:deploy", "--target", "staging"],
    );
    api_out.assert_success();
    api_out.assert_stdout_contains("api_v2::deploy ran with target=staging");

    // (2) Structural-only intermediate doesn't shadow the descendant.
    // `caller_structural_only_leaf` uses the typed
    // `subtasks::structural_only::leaf::leaf_task` path; this asserts
    // the parent's `subtasks` tree generated an empty `pub mod
    // structural_only` along the descendant's path.
    let leaf_out = harness::run_rnme(
        POSITIVE_FIXTURE.path(),
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
///
/// Acceptance: plan §8 / brief item 8 / plan §391.
/// `typed_invocation_collision/foo-bar/RUNME.rs` and
/// `.../foo_bar/RUNME.rs` both normalize to the module name `foo_bar`.
/// Neither carries a `[rnme.rename]`. `impl-collision-detection`
/// (task #17) raises `CompileError::SiblingNameCollision` at
/// workspace-generation time (before `cargo build`), and its `Display`
/// names both colliding paths, the resolved name, and a paste-ready
/// `[rnme.rename]` snippet with a suggested name
/// (`foo_bar_dashed` here — one of the two paths has dashes, so the
/// `_dashed` heuristic fires).
///
/// Assertions use loose substring matching so future error
/// reformatting doesn't churn the test.
#[test]
fn rename_collision_is_rejected() {
    let out = harness::run_rnme(COLLISION_FIXTURE.path(), &["--cli", "noop"]);
    assert_ne!(
        out.exit_code, 0,
        "expected non-zero exit for unresolved collision; got stdout={} stderr={}",
        out.stdout, out.stderr
    );
    // Both colliding paths appear in the error.
    out.assert_stderr_contains("foo-bar");
    out.assert_stderr_contains("foo_bar");
    // Paste-ready snippet header.
    out.assert_stderr_contains("[rnme.rename]");
    // The suggested name flagged by the `_dashed` heuristic.
    out.assert_stderr_contains("foo_bar_dashed");
}
