//! [rnme.rename]
//! name = "Hello World"
//!
//! Exercises the heck-normalization side of `apply-rename`: the input
//! `"Hello World"` runs through `to_snake_case` and becomes
//! `hello_world`. Resolvable via `ctx.run("hello_world:greet", &[])`
//! and the CLI as `rnme --cli hello_world:greet`.
//!
//! The on-disk directory is `HelloWorld/` so the test driver can also
//! verify that the rename produces a *different* group key than what
//! the dir name would yield natively (it would otherwise normalize to
//! `helloworld` via `Path::to_string_lossy`).

use rnme::prelude::*;

/// Trivial task whose group key should be `hello_world`.
#[rnme::task]
async fn greet(_ctx: &TaskContext) -> TaskResult {
    info!("hello_world::greet ran");
    Ok(())
}
