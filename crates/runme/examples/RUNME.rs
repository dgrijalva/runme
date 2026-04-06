use runme::prelude::*;

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

#[runme::main]
fn main() {}
