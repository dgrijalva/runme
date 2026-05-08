//! MCP frontend module.
//!
//! - [`wire`] — pure protocol types with serde derives.
//! - [`transport`] — JSONL-framed `Send`/`Recv` over a TCP loopback stream.
//! - [`engine_server`] — `--engine` daemon: an embedded engine that serves
//!   the wire protocol on a single supervisor connection.
//! - [`supervisor`] — `--mcp` outer-driver entry: spawns engine generations,
//!   routes RPC by dotted address, exposes the high-level API the rmcp tool
//!   surface plugs into.
//! - [`build`] — file-watcher and `BuildState` machine driving generation
//!   rotation on edits.
//! - [`routing`] — dotted-address parser and per-gen routing table.
//! - [`report`] — human-readable task report renderer.
//! - [`tools`] — the rmcp `ServerHandler` exposing every MCP tool.
//!
//! See `docs/mcp_design.md` for the full design.

pub mod build;
pub mod engine_server;
pub mod report;
pub mod routing;
pub mod supervisor;
pub mod tools;
pub mod transport;
pub mod wire;
