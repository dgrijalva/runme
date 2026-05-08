//! MCP frontend module.
//!
//! This module hosts the supervisor↔engine wire protocol and (in later
//! phases) the supervisor agent. Phase 2 lands [`wire`] — pure types
//! with serde derives. The transport adapter is a sibling slice; the
//! engine server, supervisor, and agent come in later phases.
//!
//! See `docs/mcp_design.md` for the full design.

pub mod build;
pub mod engine_server;
pub mod routing;
pub mod supervisor;
pub mod transport;
pub mod wire;
