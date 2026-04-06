pub mod cmd;
pub mod error;
pub mod task;
pub mod process;
pub mod signal;
pub mod prelude;

// Re-export macros at the crate root so users can write #[runme::task] and #[runme::main]
pub use runme_macros::task;
pub use runme_macros::main;

// Re-export inventory so generated code can reference it
pub use inventory;

// Re-export tokio so macro-generated code (#[tokio::main]) can reference it
pub use tokio;

// Re-export serde_json so macro-generated code can reference it
pub use serde_json;
