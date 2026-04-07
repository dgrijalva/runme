#!/usr/bin/env runme

use runme::prelude::*;

/// Install runme CLI via cargo install
#[runme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    info!("Installing runme CLI");
    ctx.exec("cargo install --path crates/runme-cli").await?;
    Ok(())
}
