//! Simple-primitives (Form-2) leaf task.
//!
//! Demonstrates the simple-primitives argument form for the typed-shim
//! macro. Not invoked from elsewhere in the fixture — its purpose is to
//! ensure Form-2 codegen works against a real RUNME.rs.

use rnme::prelude::*;

/// Build something with primitive bool args.
#[rnme::task]
async fn build(_ctx: &TaskContext, release: bool, verbose: bool) -> TaskResult {
    info!("child_a::build ran with release={} verbose={}", release, verbose);
    Ok(())
}
