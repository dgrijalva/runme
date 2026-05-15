//! Output format selection shared between the CLI dispatcher and the
//! task runtime.
//!
//! Lives outside `cli` so types like `EngineInternals` and `TaskContext`
//! can hold a hint without `execution`/`task` depending on `cli`.

/// Output format for CLI mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text (structured columns)
    Text,
    /// Raw process output (unformatted, good for piping)
    Raw,
}
