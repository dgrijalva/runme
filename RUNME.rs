#!/usr/bin/env runme

use runme::prelude::*;

/// Install runme CLI via cargo install
#[runme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    ctx.tui_wait(false);
    ctx.tui_output().stderr().subscribe(&ctx.task_output()).await;

    info!("Installing runme CLI");
    ctx.exec("cargo install --path crates/runme-cli").await?;
    info!("Done!");
    Ok(())
}

