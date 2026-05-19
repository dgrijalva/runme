//! Verifies that calling a task fn without `.await?` or `.spawn()?`
//! triggers the `unused_must_use` lint. The `TaskBuilder` returned by
//! the typed shim carries `#[must_use]`; treating the lint as an error
//! via an outer `#[deny(unused_must_use)]` on the calling fn turns
//! the bare-call site into a compile failure the test driver can
//! assert on via the rnme compile-step's stderr.
//!
//! Inner attributes can't be at file top (rnme's source transform
//! prepends `const __RNME_GROUP = ...`); using an outer attribute on
//! the calling fn keeps the deny narrow and avoids that constraint.

use rnme::prelude::*;

/// A trivial task. Used as the callee whose unused builder must trip
/// the lint.
#[rnme::task]
async fn worker(_ctx: &TaskContext) -> TaskResult {
    info!("worker ran");
    Ok(())
}

/// Calls `worker(ctx)` without `.await?` or `.spawn()?`. The shim
/// returns a `#[must_use] TaskBuilder`; under
/// `#[deny(unused_must_use)]` the dropped builder is a hard compile
/// error.
#[deny(unused_must_use)]
#[rnme::task]
async fn bare_caller(ctx: &TaskContext) -> TaskResult {
    worker(ctx);
    Ok(())
}
