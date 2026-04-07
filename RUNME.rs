#!/usr/bin/env runme

use runme::prelude::*;

/// Install runme CLI via cargo install
#[runme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    info!("Installing runme CLI");
    ctx.exec("cargo install --path crates/runme-cli").await?;
    Ok(())
}

/// Test task
#[runme::task]
fn test(_ctx: &TaskContext) -> TaskResult {
    eprintln!("test233");
    Ok(())
}

/// Testing nested runme files
#[runme::task]
async fn example_nested(ctx: &TaskContext) -> TaskResult {
    ctx.exec("echo foo bar baz").await?;
    eprintln!("It's working!0");
    Ok(())
}
