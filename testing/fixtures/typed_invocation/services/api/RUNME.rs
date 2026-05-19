//! [rnme.rename]
//! name = "api_v2"
//!
//! The on-disk directory is `api/` but the substituted-then-normalized
//! identifier is `api_v2`. This must propagate to:
//!   - the cargo crate name
//!   - the inventory group key (so `ctx.run("services/api_v2:deploy", _)` resolves)
//!   - the `subtasks` module path (so `subtasks::services::api_v2::*` resolves)
//!
//! Defines the Form-3 (parser-struct) task `deploy` plus the Form-2
//! task `health`. Exports `pub struct ApiDeployOpts` for parents to
//! construct.

use rnme::prelude::*;
use clap::Parser;

/// Options for the `deploy` task.
#[derive(Parser, Clone, Debug)]
pub struct ApiDeployOpts {
    /// Deployment target (e.g. "staging", "production").
    #[arg(long)]
    pub target: String,

    /// Whether to deploy as a canary first.
    #[arg(long)]
    pub canary: bool,
}

/// Deploy the API. Form-3 task: takes a parsed struct arg.
#[rnme::task]
async fn deploy(_ctx: &TaskContext, opts: ApiDeployOpts) -> TaskResult {
    info!(
        "api_v2::deploy ran with target={} canary={}",
        opts.target, opts.canary
    );
    Ok(())
}

/// Health-check the API on a port. Form-2 task: simple primitives.
#[rnme::task]
async fn health(_ctx: &TaskContext, port: u16) -> TaskResult {
    info!("api_v2::health ran on port {}", port);
    Ok(())
}
