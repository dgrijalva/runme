//! Built-in tasks registered under the `"builtin"` group.
//!
//! These tasks are available in any rnme project via `:list` or `builtin:list`.
//!
//! Works because `lib.rs` has `extern crate self as rnme`, which lets
//! the macro's `::rnme::` paths resolve inside the crate.

use crate::prelude::*;

const __RNME_GROUP: &str = "builtin";

/// List available tasks
#[rnme::task]
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

/// Format RUNME.rs files with rustfmt
#[rnme::task]
async fn fmt(ctx: &TaskContext) -> TaskResult {
    let files = runme_files()?;
    if files.is_empty() {
        return Err(TaskError::from("no RUNME.rs files found"));
    }
    ctx.exec(
        Cmd::new("rustfmt")
            .arg("--edition")
            .arg("2024")
            .args(&files),
    )
    .await?
    .ok()?;
    Ok(())
}

/// Type-check the generated workspace with cargo check
#[rnme::task]
async fn check(ctx: &TaskContext) -> TaskResult {
    let cache_dir = cache_dir()?;
    ctx.exec(Cmd::new("cargo").arg("check").cwd(&cache_dir))
        .await?
        .ok()?;
    Ok(())
}

/// Clean the generated workspace's build artifacts
#[rnme::task]
async fn clean(ctx: &TaskContext) -> TaskResult {
    let cache_dir = cache_dir()?;
    ctx.exec(Cmd::new("cargo").arg("clean").cwd(&cache_dir))
        .await?
        .ok()?;
    Ok(())
}

fn cache_dir() -> Result<std::path::PathBuf, TaskError> {
    std::env::var_os("RNME_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| TaskError::from("RNME_CACHE_DIR not set (run via the rnme binary)"))
}

fn runme_files() -> Result<Vec<String>, TaskError> {
    let raw = std::env::var("RNME_RUNME_FILES")
        .map_err(|_| TaskError::from("RNME_RUNME_FILES not set (run via the rnme binary)"))?;
    Ok(raw
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}
