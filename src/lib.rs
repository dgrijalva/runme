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
pub use rnme_macros::cmd;
pub use rnme_macros::init;
pub use rnme_macros::task;

// Re-export inventory so generated code can reference it
pub use inventory;

// Re-export tokio so macro-generated code (#[tokio::main]) can reference it
pub use tokio;

// Re-export serde_json so macro-generated code can reference it
pub use serde_json;

// Re-export clap so macro-generated arg_metadata code can reference it
pub use clap;
