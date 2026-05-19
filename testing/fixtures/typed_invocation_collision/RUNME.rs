//! Root for the unresolved-collision negative fixture.
//!
//! `foo-bar/` and `foo_bar/` siblings both normalize to `foo_bar`.
//! Neither carries a `[rnme.rename]`. When collision-detection lands
//! (plan task #17), build against this tree must fail with a paste-ready
//! error naming both colliding paths.

use rnme::prelude::*;

/// Trivial root task — exists so the root RUNME.rs has any task at all.
#[rnme::task]
async fn noop(_ctx: &TaskContext) -> TaskResult {
    info!("collision-root noop ran");
    Ok(())
}
