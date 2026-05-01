//! Shared task library for testing cross-crate inventory visibility.
//!
//! This crate defines tasks in a separate compilation unit from the test
//! binary. The `inventory` crate uses linker sections to register items
//! at load time, but the linker will silently strip sections from library
//! crates if no symbols are referenced from them. The `__rnme_link()`
//! function forces the linker to include this crate's translation unit,
//! making the `TaskDef` registrations visible to `Registry::from_inventory()`.
//!
//! # Pattern for shared task libraries
//!
//! 1. Define `const __RNME_GROUP: &str = "your_group";`
//! 2. Use `#[rnme::task]` on async functions as usual
//! 3. Export `pub fn __rnme_link() {}` — a no-op the consumer must call
//! 4. In the consuming crate, add this as a dependency and call
//!    `rnme_test_tasks::__rnme_link()` before `Registry::from_inventory()`

use rnme::prelude::*;

/// Group name for tasks in this shared crate.
const __RNME_GROUP: &str = "shared";

/// A shared task that greets
#[rnme::task]
async fn greet(ctx: &TaskContext) -> TaskResult {
    info!("hello from shared greet task: {}", ctx.name);
    Ok(())
}

/// A shared task with args
#[rnme::task]
async fn shared_echo(ctx: &TaskContext, message: String) -> TaskResult {
    info!("shared_echo: message={}", message);
    let _ = ctx;
    Ok(())
}

/// Reports its own group
#[rnme::task]
async fn group_check(ctx: &TaskContext) -> TaskResult {
    info!("group_check running in context: {}", ctx.name);
    Ok(())
}

/// No-op function that forces the linker to include this crate's
/// inventory registrations. The consuming binary must call this
/// (or reference any symbol from this crate) before iterating inventory.
pub fn __rnme_link() {}
