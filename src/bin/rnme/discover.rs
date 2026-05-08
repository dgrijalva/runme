//! Outer-driver thin shim around the library `rnme::discover` module.
//!
//! Discovery used to live here; it now lives in `src/discover.rs` so the
//! MCP supervisor's file-watcher (Phase 5) can share the exact same
//! lookup logic. This file simply re-exports the API so the rest of the
//! binary doesn't have to change its imports.

pub use rnme::discover::{DiscoveryResult, discover};
