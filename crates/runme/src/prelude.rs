pub use crate::cmd::Cmd;
pub use crate::error::{ExitHint, ResultExt, TaskError, TaskResult};
pub use crate::init::{GroupDef, InitContext, InitDef};
pub use crate::log::buffer::OutputBuffer;
pub use crate::log::extract::{CommonJsonFieldExtractor, FieldExtractor, LayeredExtractor};
pub use crate::log::parse::{
    CargoDiagnosticParser, FallbackParser, JsonlParser, LogfmtParser, PlainLineParser,
    RecordParser, RustPanicParser,
};
pub use crate::log::{ExtractedFields, LogEntry, ParseResult, ParsedContent, RawRecord};
pub use crate::process::{ExecOutput, ExecOutputExt, ProcessError, ProcessHandle};
pub use crate::signal::SignalHandler;
pub use crate::task::{Registry, SpawnEvent, TaskContext, TaskDef, TaskFn};
pub use runme_macros::init;
pub use runme_macros::task;

// Tracing macros for task function logging
pub use tracing::{debug, error, info, trace, warn};

// The tracing layer for wiring into a subscriber
pub use crate::tracing_layer::LogEntryLayer;

// Re-export libs
pub use tokio;
