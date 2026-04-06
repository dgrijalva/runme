use runme::prelude::*;

#[runme::task(desc = "Say hello")]
async fn hello(ctx: &TaskContext) {
    println!("Hello from task: {}", ctx.name);
}

#[runme::task(desc = "Say goodbye")]
async fn goodbye(ctx: &TaskContext) {
    println!("Goodbye from task: {}", ctx.name);
}

#[runme::main]
fn main() {}
