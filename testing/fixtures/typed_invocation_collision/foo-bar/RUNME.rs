//! Sibling whose dir name `foo-bar` normalizes to `foo_bar` — collides
//! with the `foo_bar/` sibling. No `[rnme.rename]` here, so the build
//! must reject this once collision-detection lands.

use rnme::prelude::*;

/// Trivial task — body is irrelevant; this file exists to trigger the
/// sibling-normalization collision.
#[rnme::task]
async fn from_dashed(_ctx: &TaskContext) -> TaskResult {
    info!("from_dashed ran");
    Ok(())
}
