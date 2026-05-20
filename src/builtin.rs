//! Built-in tasks registered under the `"builtin"` group.
//!
//! These tasks are available in any rnme project via `:list` or `builtin:list`.
//!
//! Works because `lib.rs` has `extern crate self as rnme`, which lets
//! the macro's `::rnme::` paths resolve inside the crate.

use crate::prelude::*;

const __RNME_GROUP: &str = "builtin";
const __RNME_DIR: &str = "";

/// List available tasks
#[rnme::task(mode = cli)]
async fn list(ctx: &TaskContext) -> TaskResult {
    use crate::ansi;
    use crate::theme::THEME;
    use std::collections::BTreeMap;

    ctx.default_format(OutputFormat::Raw);
    let Some(query) = ctx.tasks() else {
        eprintln!("No task registry available");
        return Ok(());
    };

    let tasks = query.all();
    if tasks.is_empty() {
        return Ok(());
    }

    // Bucket by group, sort tasks within each group.
    let mut by_group: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for t in tasks {
        by_group.entry(t.group.clone()).or_default().push(t);
    }
    for v in by_group.values_mut() {
        v.sort_by(|a, b| a.name.cmp(&b.name));
    }

    // Group order: root ("") first, then user groups alphabetically, then
    // "builtin" last so the locally-meaningful tasks lead.
    let mut groups: Vec<&str> = by_group.keys().map(|s| s.as_str()).collect();
    groups.sort_by_key(|g| match *g {
        "" => (0u8, ""),
        "builtin" => (2, ""),
        other => (1, other),
    });

    // Pad task names to the longest name across all groups so columns line up.
    let name_width = by_group
        .values()
        .flat_map(|v| v.iter())
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0);

    let term_width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100);
    // 2 indent + name column + 2 gap.
    let desc_width = term_width.saturating_sub(name_width + 4).max(20);

    const BOLD: &str = "\x1b[1m";
    let dim = ansi::fg(THEME.dim);
    let accent = ansi::fg(THEME.accent);
    let reset = ansi::RESET;

    let mut first = true;
    for group in groups {
        if !first {
            ctx.println("").await;
        }
        first = false;
        if !group.is_empty() {
            ctx.println(format!("{accent}{BOLD}{group}{reset}")).await;
        }
        for t in &by_group[group] {
            let desc = t
                .description
                .as_deref()
                .unwrap_or("")
                .replace('\n', " ");
            let desc = truncate_chars(&desc, desc_width);
            ctx.println(format!(
                "  {BOLD}{name:<width$}{reset}  {dim}{desc}{reset}",
                name = t.name,
                width = name_width,
            ))
            .await;
        }
    }
    Ok(())
}

/// Truncate `s` to at most `max` characters, appending `…` if truncated.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Format RUNME.rs files with rustfmt
#[rnme::task(mode = cli)]
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
#[rnme::task(mode = cli)]
async fn check(ctx: &TaskContext) -> TaskResult {
    let cache_dir = cache_dir()?;
    ctx.exec(Cmd::new("cargo").arg("check").cwd(&cache_dir))
        .await?
        .ok()?;
    Ok(())
}

/// Clean the generated workspace's build artifacts
#[rnme::task(mode = cli)]
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
