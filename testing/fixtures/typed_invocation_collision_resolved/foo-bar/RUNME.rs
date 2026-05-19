//! [rnme.rename]
//! name = "foo_bar_dashed"
//!
//! Rename disambiguates this sibling from `foo_bar/` (which normalizes
//! to `foo_bar` on its own). Without the rename, the two would collide.

use rnme::prelude::*;

/// Trivial task.
#[rnme::task]
async fn from_dashed_resolved(_ctx: &TaskContext) -> TaskResult {
    info!("from_dashed_resolved ran (group should be foo_bar_dashed)");
    Ok(())
}
