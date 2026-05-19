//! Root RUNME for the typed-invocation positive fixture.
//!
//! Verifies (across the test driver at tests/typed_invocation.rs):
//!   - in-file typed calls for all three arg forms (Form-1 zero-args,
//!     Form-2 simple primitives, Form-3 parser struct) — caller and
//!     callee both live in this RUNME.rs.
//!   - dynamic-path parity: `ctx.run("name", &[...])` resolves the same
//!     task as the typed in-file call.
//!   - cross-file typed calls into descendants via the auto-injected
//!     `subtasks::` module tree (Phase 3 / `subtasks-injection`).
//!   - descendant types are constructible from the parent
//!     (`subtasks::services::api_v2::ApiDeployOpts { .. }`).
//!   - intermediate structural-only dirs (`structural_only/` has no
//!     RUNME.rs) surface as empty `pub mod` along the path to the
//!     descendant leaf.

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
/// Exercises plan acceptance §7 — `ctx.run` and the typed in-file call
/// converge on the same registered task.
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
/// auto-injected `subtasks` module tree. Path uses the *renamed*
/// identifier `api_v2` (the descendant's on-disk dir is `api/`,
/// renamed via `[rnme.rename] name = "api_v2"`), so this also
/// transitively verifies that the rename propagates to the
/// `subtasks::` module path. Exercises plan acceptance §4.
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
/// Verifies that types `pub struct`'d in a child RUNME.rs travel with
/// the task into the parent's view. Exercises plan acceptance §5.
#[rnme::task]
async fn caller_uses_child_type(ctx: &TaskContext) -> TaskResult {
    // Construct the struct exported by `services/api/RUNME.rs` directly
    // from the parent. Compilation success alone is most of the proof
    // here; the runtime assertion adds defense-in-depth.
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
/// (`structural_only/`) has no RUNME.rs of its own. Exercises plan
/// acceptance §6 — intermediate structural-only dirs surface as empty
/// `pub mod` along the path to a descendant.
#[rnme::task]
async fn caller_structural_only_leaf(ctx: &TaskContext) -> TaskResult {
    info!("caller_structural_only_leaf: about to invoke subtasks::structural_only::leaf::leaf_task");
    subtasks::structural_only::leaf::leaf_task(ctx).await?;
    info!("caller_structural_only_leaf: leaf_task completed");
    Ok(())
}
