//! Leaf below a structural-only intermediate dir (`structural_only/`
//! has no RUNME.rs).
//!
//! Exercises the "intermediate dir without a RUNME.rs surfaces as an
//! empty structural module on the path to a descendant" property
//! (plan §3 design doc; plan brief item 1).

use rnme::prelude::*;

/// Trivial task to confirm reachability under a structural-only parent.
#[rnme::task]
async fn leaf_task(_ctx: &TaskContext) -> TaskResult {
    info!("structural_only::leaf::leaf_task ran");
    Ok(())
}
