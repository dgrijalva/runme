//! Common imports for `RUNME.rs` files.
//!
//! Glob-import this module at the top of every `RUNME.rs`:
//!
//! ```rust,ignore
//! use rnme::prelude::*;
//! ```
//!
//! It re-exports the types and macros most tasks need:
//!
//! - [`TaskContext`] — runtime context (`exec`, `spawn`, `watch`, `run`, …)
//! - [`Cmd`], [`cmd!`] — describe commands to run
//! - [`TaskResult`], [`TaskError`], [`ResultExt`], [`ExitHint`] — task error type and helpers
//! - [`InitContext`], [`init`] — per-file setup hook
//! - [`SpawnBuilder`], [`ProcessHandle`], [`ProcessResult`], [`Output`],
//!   [`Termination`], [`ProcessError`], [`ReadinessCondition`] — process control
//! - [`Watch`], [`glob_filter`] — file-system watching
//! - The [`tracing`] macros `info!`, `error!`, `warn!`, `debug!`, `trace!`
//! - The [`macro@task`] attribute macro
//!
//! Plus a handful of helper crates ([`tokio`], [`clap`], [`futures`], [`itertools`])
//! for common use cases without needing to declare them as extra dependencies.

pub use crate::cmd::Cmd;
pub use crate::error::{ExitHint, ResultExt, TaskError, TaskResult};
pub use crate::init::{GroupDef, InitContext, InitDef};
pub use crate::log::buffer::OutputBuffer;
pub use crate::log::extract::{CommonJsonFieldExtractor, FieldExtractor, LayeredExtractor};
pub use crate::log::parse::{
    CargoDiagnosticParser, FallbackParser, JsonlParser, LogfmtParser, PlainLineParser,
    RecordParser, RustPanicParser,
};
pub use crate::log::{ExtractedFields, LogEntry, ParseResult, ParsedContent, RawRecord, Stream};
pub use crate::output::OutputFormat;
pub use crate::process::{
    Output, ProcessError, ProcessHandle, ProcessResult, ReadinessCondition, SpawnBuilder,
    Termination,
};
pub use crate::signal::SignalHandler;
pub use crate::task::{
    ArgMetadataFn, DynamicTaskFn, Registry, RestartHandle, SpawnEvent, StepGuard, TaskContext,
    TaskDef, TaskFn, TaskFnKind, TaskGuard, TaskInfo, TaskQuery, UiHint,
};
pub use crate::watch::{Watch, WatchInfo, WatchKind, glob_filter};
pub use crate::spawn;
pub use rnme_macros::cmd;
pub use rnme_macros::init;
pub use rnme_macros::task;
pub use rnme_macros::task_template;
pub use rnme_macros::import_task;

// Tracing macros for task function logging
pub use tracing::{debug, error, info, trace, warn};

// The tracing layer for wiring into a subscriber
pub use crate::tracing_layer::LogEntryLayer;

// Re-export libs
pub use clap;
pub use futures;
pub use itertools::{self, Itertools};
pub use tokio;
