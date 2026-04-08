//! Shared test helpers for in-process integration tests.
//!
//! Provides utilities for collecting and inspecting OutputBuffer entries,
//! searching log output for patterns, and other common test operations.

use runme::prelude::*;

/// Collect all entries from an OutputBuffer into a Vec of LogEntry.
///
/// Usage:
/// ```ignore
/// let ctx = TaskContext::new("test");
/// // ... run task ...
/// let entries = collect_output(&ctx).await;
/// ```
pub async fn collect_output(ctx: &TaskContext) -> Vec<LogEntry> {
    ctx.output_lines().await
}

/// Search output entries for a line whose `raw` field contains the given pattern.
///
/// Returns true if at least one entry matches.
pub async fn output_contains(ctx: &TaskContext, pattern: &str) -> bool {
    let entries = ctx.output_lines().await;
    entries.iter().any(|e| e.raw.contains(pattern))
}

/// Search output entries for a line whose `message` field contains the given pattern.
///
/// Returns true if at least one entry matches.
pub async fn output_message_contains(ctx: &TaskContext, pattern: &str) -> bool {
    let entries = ctx.output_lines().await;
    entries
        .iter()
        .any(|e| e.message.as_deref().is_some_and(|m| m.contains(pattern)))
}

/// Collect all output entries whose `raw` field contains the given pattern.
pub async fn output_matching(ctx: &TaskContext, pattern: &str) -> Vec<LogEntry> {
    let entries = ctx.output_lines().await;
    entries
        .into_iter()
        .filter(|e| e.raw.contains(pattern))
        .collect()
}

/// Return the number of output entries.
pub async fn output_count(ctx: &TaskContext) -> usize {
    ctx.output_lines().await.len()
}
