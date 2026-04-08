//! Built-in tasks registered under the `"builtin"` group.
//!
//! These tasks are available in any runme project via `:list` or `builtin:list`.
//!
//! Works because `lib.rs` has `extern crate self as runme`, which lets
//! the macro's `::runme::` paths resolve inside the crate.

use crate::prelude::*;

const __RUNME_GROUP: &str = "builtin";

/// List available tasks
#[runme::task]
async fn list(ctx: &TaskContext) -> TaskResult {
    if let Some(query) = ctx.tasks() {
        for task in query.all() {
            let desc = task.description.unwrap_or("");
            if task.group.is_empty() {
                ctx.println(format!("{}: {}", task.name, desc)).await;
            } else {
                ctx.println(format!("[{}] {}: {}", task.group, task.name, desc))
                    .await;
            }
        }
    } else {
        eprintln!("No task registry available");
    }
    Ok(())
}
