//! CLI test harness for running the `rnme` binary against fixture directories.
//!
//! Provides `run_rnme()` for subprocess invocation, `CliOutput` for inspecting
//! results, and `write_fixture()` for generating RUNME.rs files in temp dirs.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// Captured output from a `rnme` invocation.
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl CliOutput {
    /// Parse stdout as JSON. Panics if stdout is not valid JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "failed to parse stdout as JSON: {}\nstdout: {}\nstderr: {}",
                e, self.stdout, self.stderr
            )
        })
    }

    /// Parse stdout as JSON, returning None if it's not valid JSON.
    pub fn try_json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.stdout).ok()
    }

    /// Assert that the command exited with code 0.
    pub fn assert_success(&self) {
        assert_eq!(
            self.exit_code, 0,
            "expected exit code 0, got {}\nstdout: {}\nstderr: {}",
            self.exit_code, self.stdout, self.stderr
        );
    }

    /// Assert a specific exit code.
    pub fn assert_exit_code(&self, code: i32) {
        assert_eq!(
            self.exit_code, code,
            "expected exit code {}, got {}\nstdout: {}\nstderr: {}",
            code, self.exit_code, self.stdout, self.stderr
        );
    }

    /// Assert stdout JSON has `{"status": "ok"}` and the given task name.
    pub fn assert_json_ok(&self, task: &str) {
        let json = self.json();
        assert_eq!(
            json.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "expected status 'ok' in JSON output: {}",
            json
        );
        assert_eq!(
            json.get("task").and_then(|v| v.as_str()),
            Some(task),
            "expected task '{}' in JSON output: {}",
            task,
            json
        );
    }

    /// Assert stdout JSON has `{"status": "error"}` and the given task name.
    pub fn assert_json_error(&self, task: &str) {
        let json = self.json();
        assert_eq!(
            json.get("status").and_then(|v| v.as_str()),
            Some("error"),
            "expected status 'error' in JSON output: {}",
            json
        );
        assert_eq!(
            json.get("task").and_then(|v| v.as_str()),
            Some(task),
            "expected task '{}' in JSON output: {}",
            task,
            json
        );
    }

    /// Assert that stdout contains a substring.
    pub fn assert_stdout_contains(&self, pattern: &str) {
        assert!(
            self.stdout.contains(pattern),
            "expected stdout to contain '{}'\nstdout: {}",
            pattern,
            self.stdout
        );
    }

    /// Assert that stderr contains a substring.
    pub fn assert_stderr_contains(&self, pattern: &str) {
        assert!(
            self.stderr.contains(pattern),
            "expected stderr to contain '{}'\nstderr: {}",
            pattern,
            self.stderr
        );
    }
}

/// Run the `rnme` binary with the given arguments, using `dir` as the working directory.
///
/// Uses `CARGO_BIN_EXE_runme` to find the binary (automatically set by cargo
/// when running integration tests for a crate that defines a `[[bin]]`).
pub fn run_rnme(dir: &Path, args: &[&str]) -> CliOutput {
    let bin = env!("CARGO_BIN_EXE_runme");
    let output = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run rnme binary at {}: {}", bin, e));

    CliOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    }
}

/// Write a file into a fixture directory, creating parent directories as needed.
///
/// `relative_path` is relative to `dir` (e.g., "RUNME.rs" or "services/RUNME.rs").
pub fn write_fixture(dir: &Path, relative_path: &str, content: &str) {
    let path = dir.join(relative_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("failed to create dir {}: {}", parent.display(), e));
    }
    std::fs::write(&path, content)
        .unwrap_or_else(|e| panic!("failed to write {}: {}", path.display(), e));
}

/// Generate RUNME.rs content for a "kitchen sink" fixture with multiple tasks.
///
/// Contains tasks that cover success, failure, arguments, and output — enough
/// for most CLI integration test scenarios.
pub fn kitchen_sink_runme() -> &'static str {
    r#"use rnme::prelude::*;

/// A task that succeeds.
#[rnme::task(desc = "Always succeeds")]
async fn succeed(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}

/// A task that fails with exit code 42.
#[rnme::task(desc = "Fails with code 42")]
async fn fail_with_code(_ctx: &TaskContext) -> TaskResult {
    Err(TaskError::from("intentional failure").with_code(42))
}

/// A task that takes arguments and logs them.
#[rnme::task(desc = "Echoes arguments")]
async fn echo_args(_ctx: &TaskContext, message: String, count: Option<u32>) -> TaskResult {
    info!("message={}, count={:?}", message, count);
    Ok(())
}

/// A task that produces stdout output via ctx.exec().
#[rnme::task(desc = "Produces output")]
async fn produce_output(ctx: &TaskContext) -> TaskResult {
    ctx.exec("echo hello-from-produce-output").await?.ok()?;
    Ok(())
}

/// A task that fails with default exit code.
#[rnme::task(desc = "Fails with default code")]
async fn fail_default(_ctx: &TaskContext) -> TaskResult {
    Err("default failure".into())
}
"#
}

/// Generate a minimal RUNME.rs that just succeeds.
pub fn minimal_runme() -> &'static str {
    r#"use rnme::prelude::*;

#[rnme::task(desc = "Minimal success task")]
async fn hello(_ctx: &TaskContext) -> TaskResult {
    Ok(())
}
"#
}

/// Generate a RUNME.rs for a subdirectory (to test group discovery).
///
/// The task name will be "sub_task" in whatever group the discovery assigns.
pub fn sub_dir_runme() -> &'static str {
    r#"use rnme::prelude::*;

#[rnme::task(desc = "A task in a subdirectory")]
async fn sub_task(_ctx: &TaskContext) -> TaskResult {
    info!("running sub_task");
    Ok(())
}
"#
}
