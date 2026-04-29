//! Task error type and conversions.
//!
//! Tasks return [`TaskResult`] (alias for `Result<(), TaskError>`). A
//! [`TaskError`] carries:
//!
//! - **Structured output** as `serde_json::Value`. Objects/arrays pass through;
//!   scalars get wrapped as `{"message": value}`. The output is what `rnme`
//!   prints, logs, and uses to derive an exit code.
//! - **An [`ExitHint`]** that suggests how `rnme` should handle the failure
//!   (specific exit code, restart, abort, or default).
//!
//! # Constructing errors
//!
//! Anything that implements [`Serialize`] (including `&str`, `String`,
//! `serde_json::Value`, your own structs) converts via [`From`]:
//!
//! ```rust,ignore
//! return Err("connection refused".into());
//! return Err(serde_json::json!({"step": "compile", "code": 1}).into());
//! ```
//!
//! For arbitrary errors that don't implement `Serialize` (`io::Error`,
//! `reqwest::Error`, …), use [`ResultExt::task_err`]:
//!
//! ```rust,ignore
//! let body = std::fs::read_to_string("config.toml").task_err()?;
//! ```
//!
//! Add an exit hint with [`TaskError::with_hint`] or [`TaskError::with_code`]:
//!
//! ```rust,ignore
//! return Err(TaskError::from("unrecoverable").with_hint(ExitHint::Abort));
//! return Err(TaskError::from("not found").with_code(127));
//! ```

use serde::Serialize;

/// Result type returned by `#[rnme::task]` functions.
///
/// Alias for `Result<(), TaskError>`.
pub type TaskResult = Result<(), TaskError>;

/// Hint to rnme about how to handle a task error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitHint {
    /// Let rnme decide (default behavior).
    Default,
    /// Suggest a specific exit code.
    Code(i32),
    /// Hint that the task should be retried.
    Restart,
    /// Hint that all tasks should be stopped.
    Abort,
}

/// Error type returned by task functions.
///
/// Carries structured (JSON) output and an optional hint for rnme's
/// shutdown/exit handling. The output is always a JSON value, normalized
/// so that objects and arrays pass through directly while scalars (strings,
/// numbers, etc.) are wrapped as `{"message": value}`.
///
/// # Construction
///
/// From any `Serialize` type (including `&str`, `String`):
/// ```ignore
/// return Err("something failed".into());
/// return Err(json!({"step": "compile", "code": 1}).into());
/// ```
///
/// From standard error types using `.task_err()`:
/// ```ignore
/// let output = std::fs::read_to_string("file.txt").task_err()?;
/// ```
///
/// With exit hints:
/// ```ignore
/// return Err(TaskError::from("fatal").with_hint(ExitHint::Abort));
/// return Err(TaskError::from("not found").with_code(127));
/// ```
// NOTE: TaskError intentionally does NOT implement Serialize.
// Doing so would conflict with the blanket From<T: Serialize> impl.
pub struct TaskError {
    output: serde_json::Value,
    hint: ExitHint,
}

impl TaskError {
    /// Create a TaskError from any `Display` type. The message is
    /// wrapped as `{"message": "..."}`.
    pub fn from_display(err: impl std::fmt::Display) -> Self {
        TaskError {
            output: serde_json::json!({"message": err.to_string()}),
            hint: ExitHint::Default,
        }
    }

    /// Set the exit hint (builder pattern).
    pub fn with_hint(mut self, hint: ExitHint) -> Self {
        self.hint = hint;
        self
    }

    /// Shorthand for `with_hint(ExitHint::Code(code))`.
    pub fn with_code(self, code: i32) -> Self {
        self.with_hint(ExitHint::Code(code))
    }

    /// Access the structured output.
    pub fn output(&self) -> &serde_json::Value {
        &self.output
    }

    /// Access the exit hint.
    pub fn hint(&self) -> &ExitHint {
        &self.hint
    }

    /// Suggested exit code based on the hint.
    pub fn exit_code(&self) -> i32 {
        match &self.hint {
            ExitHint::Default | ExitHint::Restart => 1,
            ExitHint::Code(c) => *c,
            ExitHint::Abort => 2,
        }
    }
}

impl From<crate::process::ProcessError> for TaskError {
    fn from(err: crate::process::ProcessError) -> Self {
        match err {
            crate::process::ProcessError::Spawn(_) => TaskError {
                output: serde_json::json!({"message": err.to_string()}),
                hint: ExitHint::Code(127),
            },
            crate::process::ProcessError::Timeout => TaskError {
                output: serde_json::json!({"message": err.to_string()}),
                hint: ExitHint::Code(124),
            },
            _ => TaskError {
                output: serde_json::json!({"message": err.to_string()}),
                hint: ExitHint::Default,
            },
        }
    }
}

impl From<crate::process::ProcessResult> for TaskError {
    fn from(result: crate::process::ProcessResult) -> Self {
        let code = result.exit_code();
        let message = format!("process {}", result.termination());
        TaskError {
            output: serde_json::json!({
                "message": message,
                "exit_code": code,
            }),
            hint: ExitHint::Code(code),
        }
    }
}

/// Blanket conversion from any `Serialize` type.
///
/// - Objects and arrays are stored directly as the output value.
/// - Scalars (strings, numbers, booleans, null) are wrapped as `{"message": value}`.
impl<T: Serialize> From<T> for TaskError {
    fn from(value: T) -> Self {
        let output = serde_json::to_value(&value).unwrap_or_else(
            |e| serde_json::json!({"message": format!("serialization failed: {}", e)}),
        );
        let output = match output {
            serde_json::Value::Object(_) | serde_json::Value::Array(_) => output,
            scalar => serde_json::json!({"message": scalar}),
        };
        TaskError {
            output,
            hint: ExitHint::Default,
        }
    }
}

impl std::fmt::Debug for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskError")
            .field("output", &self.output)
            .field("hint", &self.hint)
            .finish()
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(msg) = self.output.get("message").and_then(|v| v.as_str()) {
            write!(f, "{}", msg)
        } else {
            write!(f, "{}", self.output)
        }
    }
}

/// Extension trait for converting `Result<T, E: Display>` to `Result<T, TaskError>`.
///
/// Useful for standard error types (io::Error, etc.) that don't implement Serialize:
/// ```ignore
/// let content = std::fs::read_to_string("file.txt").task_err()?;
/// let status = Command::new("cargo").status().task_err()?;
/// ```
pub trait ResultExt<T> {
    fn task_err(self) -> Result<T, TaskError>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for Result<T, E> {
    fn task_err(self) -> Result<T, TaskError> {
        self.map_err(|e| TaskError::from_display(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_str() {
        let err: TaskError = "something failed".into();
        assert_eq!(err.output(), &json!({"message": "something failed"}));
        assert_eq!(err.hint(), &ExitHint::Default);
        assert_eq!(err.to_string(), "something failed");
    }

    #[test]
    fn from_string() {
        let err: TaskError = String::from("bad thing").into();
        assert_eq!(err.output(), &json!({"message": "bad thing"}));
    }

    #[test]
    fn from_json_object() {
        let err: TaskError = json!({"step": "compile", "code": 1}).into();
        assert_eq!(err.output(), &json!({"step": "compile", "code": 1}));
    }

    #[test]
    fn from_json_array() {
        let err: TaskError = json!(["error1", "error2"]).into();
        assert_eq!(err.output(), &json!(["error1", "error2"]));
    }

    #[test]
    fn from_number_wraps() {
        let err: TaskError = 42i32.into();
        assert_eq!(err.output(), &json!({"message": 42}));
    }

    #[test]
    fn from_serializable_struct() {
        #[derive(Serialize)]
        struct Info {
            step: String,
            detail: String,
        }
        let err: TaskError = Info {
            step: "build".into(),
            detail: "missing dep".into(),
        }
        .into();
        assert_eq!(
            err.output(),
            &json!({"step": "build", "detail": "missing dep"})
        );
    }

    #[test]
    fn with_hint() {
        let err = TaskError::from("fail").with_hint(ExitHint::Abort);
        assert_eq!(err.hint(), &ExitHint::Abort);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn with_code() {
        let err = TaskError::from("not found").with_code(127);
        assert_eq!(err.hint(), &ExitHint::Code(127));
        assert_eq!(err.exit_code(), 127);
    }

    #[test]
    fn display_message_field() {
        let err: TaskError = "hello".into();
        assert_eq!(format!("{}", err), "hello");
    }

    #[test]
    fn display_object_without_message() {
        let err: TaskError = json!({"code": 1}).into();
        assert_eq!(format!("{}", err), r#"{"code":1}"#);
    }

    #[test]
    fn task_err_extension() {
        let result: Result<(), std::io::Error> =
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        let converted = result.task_err();
        assert!(converted.is_err());
        let err = converted.unwrap_err();
        assert_eq!(err.to_string(), "gone");
    }

    #[test]
    fn exit_code_defaults() {
        assert_eq!(TaskError::from("x").exit_code(), 1);
        assert_eq!(
            TaskError::from("x")
                .with_hint(ExitHint::Restart)
                .exit_code(),
            1
        );
        assert_eq!(
            TaskError::from("x").with_hint(ExitHint::Abort).exit_code(),
            2
        );
        assert_eq!(TaskError::from("x").with_code(42).exit_code(), 42);
    }
}
