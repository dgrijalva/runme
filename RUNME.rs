use rnme::prelude::*;

/// Install rnme CLI via cargo install
#[rnme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    info!("Installing rnme CLI");
    ctx.exec("cargo install --path .").await?;
    info!("Done!");
    Ok(())
}
