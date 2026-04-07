// Example RUNME.rs demonstrating the runme task system.
//
// In the real pipeline, the code generator injects __RUNME_GROUP and provides
// a generated main(). For standalone compilation (examples, docs), we define
// them manually here.
//
// NOTE: This example uses manual inventory::submit! rather than #[runme::task]
// because the macro will emit `group: __RUNME_GROUP` only after Phase 2 (macro
// update). Until then, use manual registration to keep the example compiling.
use runme::prelude::*;
use std::future::Future;
use std::pin::Pin;

const __RUNME_GROUP: &str = "";

async fn hello(ctx: &TaskContext) {
    println!("Hello from task: {}", ctx.name);
}

async fn goodbye(ctx: &TaskContext) {
    println!("Goodbye from task: {}", ctx.name);
}

fn hello_wrapper(
    ctx: &TaskContext,
) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
    Box::pin(async move {
        hello(ctx).await;
        Ok(())
    })
}

fn goodbye_wrapper(
    ctx: &TaskContext,
) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>> {
    Box::pin(async move {
        goodbye(ctx).await;
        Ok(())
    })
}

runme::inventory::submit! {
    TaskDef {
        name: "hello",
        description: Some("Say hello"),
        group: __RUNME_GROUP,
        watch: None,
        depends_on: &[],
        func: hello_wrapper,
    }
}

runme::inventory::submit! {
    TaskDef {
        name: "goodbye",
        description: Some("Say goodbye"),
        group: __RUNME_GROUP,
        watch: None,
        depends_on: &[],
        func: goodbye_wrapper,
    }
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
