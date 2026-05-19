//! Sibling whose dir name `foo_bar` normalizes to `foo_bar`. No
//! rename needed — the other sibling (`foo-bar/`) was renamed to
//! `foo_bar_dashed`.

use rnme::prelude::*;

/// Trivial task.
#[rnme::task]
async fn from_undered_resolved(_ctx: &TaskContext) -> TaskResult {
    info!("from_undered_resolved ran (group should be foo_bar)");
    Ok(())
}
