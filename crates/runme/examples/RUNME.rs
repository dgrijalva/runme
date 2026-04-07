// Example RUNME.rs demonstrating the runme task system.
//
// In the real pipeline, the code generator injects __RUNME_GROUP and provides
// a generated main(). For standalone compilation (examples, docs), we define
// them manually here.
use runme::prelude::*;

const __RUNME_GROUP: &str = "";

#[runme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("Example Tasks");
}

/// Say hello
#[runme::task]
async fn hello(ctx: &TaskContext) {
    println!("Hello from task: {}", ctx.name);
}

/// Say goodbye
#[runme::task]
async fn goodbye(ctx: &TaskContext) {
    println!("Goodbye from task: {}", ctx.name);
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
