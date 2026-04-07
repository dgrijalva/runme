use runme::prelude::*;

const __RUNME_GROUP: &str = "";

/// Install runme CLI via cargo install
#[runme::task]
async fn install(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo install --path crates/runme-cli").await?;
    Ok(())
}

/// Test task
#[runme::task]
fn test(_ctx: &TaskContext) -> TaskResult {
    eprintln!("test233");
    Ok(())
}

fn main() {
    runme::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(async {
            let registry = Registry::from_inventory();
            let args: Vec<String> = std::env::args().collect();

            if args.iter().any(|a| a == "--list") {
                for task in registry.list() {
                    println!("{}: {}", task.name, task.description.unwrap_or(""));
                }
                return;
            }

            if let Some(task_name) = args.get(1) {
                if let Err(e) = registry.run(task_name).await {
                    eprintln!("Error: {}", e);
                    std::process::exit(e.exit_code());
                }
            } else {
                println!("Available tasks:");
                for task in registry.list() {
                    println!("  {}: {}", task.name, task.description.unwrap_or(""));
                }
            }
        });
}
