use runme::prelude::*;

/// Install runme CLI via cargo install
#[runme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo install --path crates/runme-cli").await?;
    Ok(())
}

#[runme::main]
fn main() {}

/// Test task
#[runme::task]
fn test(_ctx: &TaskContext) -> TaskResult {
    eprintln!("test2333");
    Ok(())
}
