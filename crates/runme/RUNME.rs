//! [dependencies]
//! heck = "*"

use heck::ToSnakeCase;
use runme::prelude::*;

#[runme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("lib");
}

/// Testing nested runme files
#[runme::task]
async fn example_nested(ctx: &TaskContext) -> TaskResult {
    ctx.exec("echo foo bar baz").await?;
    eprintln!("It's working! {}", "FooBar".to_snake_case());
    Ok(())
}
