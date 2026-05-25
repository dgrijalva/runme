//! Fixture crate for validating `#[rnme::task_template]` library-side emission.
//!
//! Templates must live at the library crate root because the per-task stamp
//! macro references the body via `$crate::__rnme_body_<name>` (no module
//! path). The fixture defines one template per supported argument form:
//!
//!   - `noop`    — zero-arg (Form 1)
//!   - `echo`    — simple primitive args (Form 2)
//!   - `build`   — clap parser struct (Form 3)
//!
//! This crate intentionally does NOT define `__RNME_GROUP` / `__RNME_DIR`.
//! Templates do not register tasks at the library site; registration only
//! happens at the consumer's stamp call. T2V validates that no `TaskDef`
//! static / no `inventory::submit!` are emitted at this library site.

use rnme::prelude::*;

/// Zero-arg template — does nothing, useful as a smoke test.
#[rnme::task_template]
async fn noop(ctx: &TaskContext) -> TaskResult {
    let _ = ctx;
    Ok(())
}

/// Simple-args template: echoes a message a configurable number of times.
#[rnme::task_template]
async fn echo(ctx: &TaskContext, message: String, count: u32, loud: bool) -> TaskResult {
    let _ = ctx;
    let _ = (message, count, loud);
    Ok(())
}

/// Clap parser-struct args.
#[derive(clap::Parser)]
pub struct BuildArgs {
    #[arg(long)]
    pub release: bool,
    #[arg(long)]
    pub target: Option<String>,
}

/// Parser-struct template: parses a `BuildArgs` from string args.
#[rnme::task_template]
async fn build(ctx: &TaskContext, args: BuildArgs) -> TaskResult {
    let _ = (ctx, args);
    Ok(())
}

/// Linker anchor — kept symmetric with the spike crate. Consumers calling
/// this is harmless; templates don't register at the library site so this
/// anchor doesn't actually pull in any inventory items from this crate.
pub fn __rnme_link() {}

