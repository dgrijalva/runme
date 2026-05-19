//! Sibling whose dir name `foo_bar` is already in normalized form — but
//! collides with `foo-bar/` (which also normalizes to `foo_bar`).

use rnme::prelude::*;

/// Trivial task.
#[rnme::task]
async fn from_undered(_ctx: &TaskContext) -> TaskResult {
    info!("from_undered ran");
    Ok(())
}
