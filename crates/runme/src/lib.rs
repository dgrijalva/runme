pub mod cmd;
pub mod error;
pub mod init;
pub mod log;
pub mod prelude;
pub mod process;
pub mod signal;
pub mod task;
pub mod tracing_layer;
pub mod tui;

// Re-export macros at the crate root so users can write #[runme::task], #[runme::init]
pub use runme_macros::init;
pub use runme_macros::task;

// Re-export inventory so generated code can reference it
pub use inventory;

// Re-export tokio so macro-generated code (#[tokio::main]) can reference it
pub use tokio;

// Re-export serde_json so macro-generated code can reference it
pub use serde_json;
