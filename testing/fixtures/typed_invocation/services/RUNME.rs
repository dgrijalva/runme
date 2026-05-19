//! Intermediate-tier RUNME — both has its own tasks and descendants.
//!
//! Presence of this file exercises plan acceptance §6 ("intermediate
//! RUNME.rs additions don't break ancestor `subtasks::descendant::...`
//! paths"). The root still calls `subtasks::services::api_v2::deploy`
//! *through* this intermediate.

use rnme::prelude::*;

/// Logs an overview message. Smoke task for the intermediate tier.
#[rnme::task]
async fn services_overview(_ctx: &TaskContext) -> TaskResult {
    info!("services_overview ran");
    Ok(())
}
