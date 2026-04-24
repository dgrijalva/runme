use rnme::prelude::*;

/// Install rnme CLI via cargo install
#[rnme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    ctx.tui_wait(false);
    ctx.tui_output().stderr().subscribe(&ctx.task_output()).await;

    info!("Installing rnme CLI");
    ctx.exec("cargo install --path .").await?;
    info!("Done!");
    Ok(())
}
