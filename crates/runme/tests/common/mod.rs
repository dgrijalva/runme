//! Shared test helpers for in-process integration tests.

use runme::prelude::*;

/// Search output entries for a line whose `raw` field contains the given pattern.
///
/// Returns true if at least one entry matches.
pub async fn output_contains(ctx: &TaskContext, pattern: &str) -> bool {
    let entries = ctx.output_lines().await;
    entries.iter().any(|e| e.raw.contains(pattern))
}
