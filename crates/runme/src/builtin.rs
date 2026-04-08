//! Built-in tasks registered under the `"builtin"` group.
//!
//! These tasks are available in any runme project via `:list` or `builtin:list`.
//!
//! Built-in tasks are registered manually (not via `#[runme::task]`) because
//! the proc macro generates `::runme::` paths which don't resolve inside
//! the `runme` crate itself.

use std::future::Future;
use std::pin::Pin;

use crate::error::TaskError;
use crate::task::{TaskContext, TaskDef, UiHint};

/// The built-in `list` task: enumerate all registered tasks.
fn list_task<'a>(
    ctx: &'a TaskContext,
    _args: &[String],
) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
    Box::pin(async move {
        if let Some(query) = ctx.tasks() {
            for task in query.all() {
                let desc = task.description.unwrap_or("");
                if task.group.is_empty() {
                    ctx.println(format!("{}: {}", task.name, desc)).await;
                } else {
                    ctx.println(format!("[{}] {}: {}", task.group, task.name, desc)).await;
                }
            }
        } else {
            eprintln!("No task registry available");
        }
        Ok(())
    })
}

fn no_arg_metadata() -> Option<clap::Command> {
    None
}

inventory::submit! {
    TaskDef {
        name: "list",
        description: Some("List available tasks"),
        group: "builtin",
        depends_on: &[],
        func: list_task,
        arg_metadata: no_arg_metadata,
        ui_hint: Some(UiHint::Cli),
    }
}
