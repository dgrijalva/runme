#![doc = include_str!("../README.md")]
//!
//! # Crate layout
//!
//! Most code in `RUNME.rs` files only needs:
//!
//! ```rust,ignore
//! use rnme::prelude::*;
//! ```
//!
//! See [`prelude`] for the full list of re-exports. The most commonly
//! used types are:
//!
//! - [`task::TaskContext`] — runtime context passed to every task
//! - [`cmd::Cmd`] and the [`cmd!`] macro — describe commands to run
//! - [`error::TaskResult`] / [`error::TaskError`] — task return type
//! - [`process::ProcessHandle`], [`process::ProcessResult`] — spawned processes
//! - [`init::InitContext`] — per-file setup hook
//! - [`watch::Watch`] — file-system watching helpers
//!
//! Procedural macros [`macro@task`], [`macro@init`], and [`cmd!`] are
//! re-exported at the crate root so you can write `#[rnme::task]` directly.

// Allow the #[rnme::task] macro to work inside this crate.
// The macro expands to `::rnme::task::TaskDef` etc., which requires
// `rnme` to be a resolvable crate name. This self-import provides that.
extern crate self as rnme;

pub mod ansi;
pub mod builtin;
pub mod cli;
pub mod cmd;
pub mod error;
pub mod execution;
pub mod init;
pub mod log;
pub mod prelude;
pub mod process;
pub mod signal;
pub mod task;
pub mod theme;
pub mod tracing_layer;
pub mod tui;
pub mod watch;

// Re-export macros at the crate root so users can write #[rnme::task], #[rnme::init]

/// Build a structured [`Cmd`](crate::cmd::Cmd) from shell-like syntax.
///
/// Whitespace separates arguments, `{expr}` interpolates a Rust expression
/// as a single argument, and `"..."` literals stay as one argument. No shell
/// is invoked.
///
/// ```rust,ignore
/// let url = "http://example.com";
/// rnme::cmd!(curl -X POST {&url} -H "Content-Type: application/json")
/// ```
pub use rnme_macros::cmd;

/// Per-file initialization hook attribute.
///
/// See the [`init`](mod@crate::init) module for the runtime API and examples.
pub use rnme_macros::init;

/// Define an `rnme` task.
///
/// See the [`task`](mod@crate::task) module for the runtime model and the three
/// argument forms (zero-arg, simple flags, clap parser struct).
pub use rnme_macros::task;

// Re-export inventory so generated code can reference it
pub use inventory;

// Re-export tokio so macro-generated code (#[tokio::main]) can reference it
pub use tokio;

// Re-export serde_json so macro-generated code can reference it
pub use serde_json;

// Re-export clap so macro-generated arg_metadata code can reference it
pub use clap;
