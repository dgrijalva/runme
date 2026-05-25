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
pub mod discover;
pub mod error;
pub mod execution;
pub mod init;
pub mod log;
pub mod mcp;
pub mod output;
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

/// Define a reusable task *template* in a regular Rust crate.
///
/// Unlike `#[rnme::task]`, a template does not self-register. It produces the
/// building blocks a consumer RUNME.rs can re-stamp into a local typed task
/// registration via `rnme::import_task!`. See `docs/task_templates.md`.
pub use rnme_macros::task_template;

/// Stamp a task template into the current scope as a typed task registration.
///
/// `rnme::import_task!(lib_crate::name);` produces all the artifacts a local
/// `#[rnme::task]` would emit (typed shim, string-args wrapper, `TaskDef`
/// static, `inventory::submit!`), using the consumer's `__RNME_GROUP` /
/// `__RNME_DIR`. The template body / argmeta live in the library crate.
/// See `docs/task_templates.md`.
pub use rnme_macros::import_task;

/// Spawn a future on the tokio runtime with the current `tracing` span
/// re-entered inside it. Use this from inside a task body whenever you'd
/// reach for `tokio::spawn(...)` — events emitted from the spawned future
/// will be attributed to the originating task. Plain `tokio::spawn` drops
/// the span context, so its events are routed nowhere and disappear from
/// the log viewer.
///
/// ```rust,ignore
/// use rnme::prelude::*;
///
/// #[rnme::task]
/// async fn worker(_ctx: &TaskContext) -> TaskResult {
///     let handle = rnme::spawn!(async move {
///         info!("from a spawned future");
///     });
///     handle.await.ok();
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! spawn {
    ($future:expr) => {
        $crate::tokio::spawn($crate::tracing::Instrument::instrument(
            $future,
            $crate::tracing::Span::current(),
        ))
    };
}

// Re-export inventory so generated code can reference it
pub use inventory;

// Re-export tokio so macro-generated code (#[tokio::main]) can reference it
pub use tokio;

// Re-export tracing so the `spawn!` macro can resolve `Instrument` / `Span`
// through `$crate::tracing` — the generated runner crate doesn't depend
// on `tracing` directly.
pub use tracing;

// Re-export serde_json so macro-generated code can reference it
pub use serde_json;

// Re-export clap so macro-generated arg_metadata code can reference it
pub use clap;
