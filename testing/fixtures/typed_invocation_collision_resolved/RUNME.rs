//! Root for the resolved-collision positive fixture (plan §391).
//!
//! Same directory pair as `typed_invocation_collision/`, with the
//! `foo-bar/` sibling carrying `[rnme.rename] name = "foo_bar_dashed"`.
//! Build must succeed: the two normalized identifiers (`foo_bar_dashed`
//! and `foo_bar`) no longer collide.

use rnme::prelude::*;

/// Trivial root task.
#[rnme::task]
async fn noop(_ctx: &TaskContext) -> TaskResult {
    info!("collision-resolved-root noop ran");
    Ok(())
}
