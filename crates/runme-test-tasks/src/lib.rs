//! Shared task library for testing cross-crate inventory visibility.
//!
//! This crate defines tasks in a separate compilation unit from the test
//! binary. The `inventory` crate uses linker sections to register items
//! at load time, but the linker will silently strip sections from library
//! crates if no symbols are referenced from them. The `__runme_link()`
//! function forces the linker to include this crate's translation unit,
//! making the `TaskDef` registrations visible to `Registry::from_inventory()`.
//!
//! # Pattern for shared task libraries
//!
//! 1. Define `const __RUNME_GROUP: &str = "your_group";`
//! 2. Use `#[runme::task]` on async functions as usual
//! 3. Export `pub fn __runme_link() {}` — a no-op the consumer must call
//! 4. In the consuming crate, add this as a dependency and call
//!    `runme_test_tasks::__runme_link()` before `Registry::from_inventory()`

use runme::prelude::*;

/// Group name for tasks in this shared crate.
const __RUNME_GROUP: &str = "shared";

/// A simple task that always succeeds.
#[runme::task(desc = "A shared task that greets")]
async fn greet(ctx: &TaskContext) -> TaskResult {
    info!("hello from shared greet task: {}", ctx.name);
    Ok(())
}

/// A task with arguments to test cross-crate arg handling.
#[runme::task(desc = "A shared task with args")]
async fn shared_echo(ctx: &TaskContext, message: String) -> TaskResult {
    info!("shared_echo: message={}", message);
    let _ = ctx;
    Ok(())
}

/// A task in the shared group to verify group assignment.
#[runme::task(desc = "Reports its own group")]
async fn group_check(ctx: &TaskContext) -> TaskResult {
    info!("group_check running in context: {}", ctx.name);
    Ok(())
}

/// No-op function that forces the linker to include this crate's
/// inventory registrations. The consuming binary must call this
/// (or reference any symbol from this crate) before iterating inventory.
pub fn __runme_link() {}
