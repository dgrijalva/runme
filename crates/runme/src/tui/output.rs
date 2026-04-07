//! Post-TUI output buffer.
//!
//! When the TUI is running, it owns stdio (alternate screen). Task code can
//! stage output into a `TuiOutput` buffer that gets flushed to real stdout/stderr
//! after the TUI closes.
//!
//! The API supports two modes:
//! - `append(&Output)` — snapshot current entries from a process output
//! - `subscribe(&Output)` — follow live output until flush
//!
//! Both can be targeted to stdout, stderr, or preserve the original stream mapping.

use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};

use crate::log::{LogEntry, Stream};
use crate::process::Output;

/// Which stream to route entries to when flushing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetStream {
    /// Keep the entry's original stream mapping.
    Preserve,
    /// Force all entries to stdout.
    Stdout,
    /// Force all entries to stderr.
    Stderr,
}

/// A captured entry with its target stream assignment.
#[derive(Clone)]
struct StagedEntry {
    entry: LogEntry,
    target: TargetStream,
}

/// An active subscription to live output.
struct Subscription {
    rx: broadcast::Receiver<LogEntry>,
    target: TargetStream,
}

/// Buffer that collects output to be written to real stdio after the TUI closes.
///
/// Not intended to be used directly — use `TuiOutputHandle` instead, which
/// manages the `Arc<Mutex<TuiOutput>>` locking.
pub struct TuiOutput {
    /// Entries captured via `append()`.
    entries: Vec<StagedEntry>,
    /// Active subscriptions from `subscribe()`.
    subscriptions: Vec<Subscription>,
}

impl TuiOutput {
    /// Create a new empty TuiOutput buffer.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            subscriptions: Vec::new(),
        }
    }

    /// Copy current entries from an Output snapshot, preserving stdout/stderr mapping.
    pub async fn append(&mut self, output: &Output) {
        self.append_with_target(output, TargetStream::Preserve).await;
    }

    /// Subscribe to live output, preserving stdout/stderr mapping.
    pub async fn subscribe(&mut self, output: &Output) {
        self.subscribe_with_target(output, TargetStream::Preserve).await;
    }

    /// Get a stream-targeted handle for stdout.
    pub fn stdout(&mut self) -> TuiOutputStream<'_> {
        TuiOutputStream {
            tui_output: self,
            target: TargetStream::Stdout,
        }
    }

    /// Get a stream-targeted handle for stderr.
    pub fn stderr(&mut self) -> TuiOutputStream<'_> {
        TuiOutputStream {
            tui_output: self,
            target: TargetStream::Stderr,
        }
    }

    /// Write literal text to stdout.
    pub fn write_stdout(&mut self, text: &str) {
        self.write_literal(text, TargetStream::Stdout);
    }

    /// Write literal text to stderr.
    pub fn write_stderr(&mut self, text: &str) {
        self.write_literal(text, TargetStream::Stderr);
    }

    /// Internal: append entries with a specific target.
    async fn append_with_target(&mut self, output: &Output, target: TargetStream) {
        let entries = output.entries().await;
        for entry in entries {
            self.entries.push(StagedEntry {
                entry,
                target,
            });
        }
    }

    /// Internal: subscribe with a specific target.
    async fn subscribe_with_target(&mut self, output: &Output, target: TargetStream) {
        let rx = output.subscribe().await;
        self.subscriptions.push(Subscription { rx, target });
    }

    /// Internal: write literal text as a staged entry.
    fn write_literal(&mut self, text: &str, target: TargetStream) {
        // Create a synthetic LogEntry for the literal text
        let entry = LogEntry {
            raw: text.to_string(),
            parsed: crate::log::ParsedContent::PlainText,
            source: String::new(),
            seq: 0,
            received_at: chrono::Utc::now(),
            timestamp: None,
            level: None,
            message: Some(text.to_string()),
            fields: std::collections::HashMap::new(),
            stream: match target {
                TargetStream::Stdout => Some(Stream::Stdout),
                TargetStream::Stderr => Some(Stream::Stderr),
                TargetStream::Preserve => None,
            },
        };
        self.entries.push(StagedEntry { entry, target });
    }

    /// Drain all subscriptions and combine with appended entries.
    ///
    /// Returns `(stdout_text, stderr_text)` — the raw text to write to
    /// real stdout and stderr after the TUI closes.
    pub async fn flush(&mut self) -> (String, String) {
        // Drain all active subscriptions
        for sub in &mut self.subscriptions {
            loop {
                match sub.rx.try_recv() {
                    Ok(entry) => {
                        self.entries.push(StagedEntry {
                            entry,
                            target: sub.target,
                        });
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => break,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        // Lost some entries, continue with what we can get
                        continue;
                    }
                }
            }
        }
        self.subscriptions.clear();

        let mut stdout_text = String::new();
        let mut stderr_text = String::new();

        for staged in self.entries.drain(..) {
            let text = &staged.entry.raw;
            match staged.target {
                TargetStream::Stdout => {
                    stdout_text.push_str(text);
                    if !text.ends_with('\n') {
                        stdout_text.push('\n');
                    }
                }
                TargetStream::Stderr => {
                    stderr_text.push_str(text);
                    if !text.ends_with('\n') {
                        stderr_text.push('\n');
                    }
                }
                TargetStream::Preserve => {
                    // Route based on the entry's original stream
                    let target = match staged.entry.stream {
                        Some(Stream::Stdout) => &mut stdout_text,
                        Some(Stream::Stderr) => &mut stderr_text,
                        // Default to stderr for entries without a stream (e.g., tracing)
                        None => &mut stderr_text,
                    };
                    target.push_str(text);
                    if !text.ends_with('\n') {
                        target.push('\n');
                    }
                }
            }
        }

        (stdout_text, stderr_text)
    }
}

impl Default for TuiOutput {
    fn default() -> Self {
        Self::new()
    }
}

/// A stream-targeted view into `TuiOutput`.
///
/// Created by `TuiOutput::stdout()` or `TuiOutput::stderr()`. All operations
/// on this handle force entries to the target stream.
pub struct TuiOutputStream<'a> {
    tui_output: &'a mut TuiOutput,
    target: TargetStream,
}

impl<'a> TuiOutputStream<'a> {
    /// Copy current entries from an Output snapshot, forcing them to this stream.
    pub async fn append(&mut self, output: &Output) {
        self.tui_output.append_with_target(output, self.target).await;
    }

    /// Subscribe to live output, forcing entries to this stream.
    pub async fn subscribe(&mut self, output: &Output) {
        self.tui_output.subscribe_with_target(output, self.target).await;
    }

    /// Write literal text to this stream.
    pub fn write(&mut self, text: &str) {
        self.tui_output.write_literal(text, self.target);
    }
}

/// A handle to a shared `TuiOutput` behind `Arc<Mutex<>>`.
///
/// Each method call acquires the lock independently, so the handle can be
/// cloned and used across async boundaries without holding a lock across awaits.
#[derive(Clone)]
pub struct TuiOutputHandle(Arc<Mutex<TuiOutput>>);

impl TuiOutputHandle {
    /// Create a new handle wrapping the given shared TuiOutput.
    pub fn new(inner: Arc<Mutex<TuiOutput>>) -> Self {
        Self(inner)
    }

    /// Copy current entries from an Output snapshot, preserving stream mapping.
    pub async fn append(&self, output: &Output) {
        self.0.lock().await.append(output).await;
    }

    /// Subscribe to live output, preserving stream mapping.
    pub async fn subscribe(&self, output: &Output) {
        self.0.lock().await.subscribe(output).await;
    }

    /// Get a stream-targeted handle for stdout.
    pub fn stdout(&self) -> TuiOutputStreamHandle {
        TuiOutputStreamHandle {
            inner: self.0.clone(),
            target: TargetStream::Stdout,
        }
    }

    /// Get a stream-targeted handle for stderr.
    pub fn stderr(&self) -> TuiOutputStreamHandle {
        TuiOutputStreamHandle {
            inner: self.0.clone(),
            target: TargetStream::Stderr,
        }
    }

    /// Write literal text to stdout.
    pub async fn write_stdout(&self, text: &str) {
        self.0.lock().await.write_stdout(text);
    }

    /// Write literal text to stderr.
    pub async fn write_stderr(&self, text: &str) {
        self.0.lock().await.write_stderr(text);
    }

    /// Flush all staged output and return (stdout_text, stderr_text).
    pub async fn flush(&self) -> (String, String) {
        self.0.lock().await.flush().await
    }

    /// Access the underlying Arc for sharing with other components.
    pub fn inner(&self) -> &Arc<Mutex<TuiOutput>> {
        &self.0
    }
}

/// A stream-targeted handle to a shared TuiOutput.
///
/// Each method call acquires the lock independently. Created by
/// `TuiOutputHandle::stdout()` or `TuiOutputHandle::stderr()`.
#[derive(Clone)]
pub struct TuiOutputStreamHandle {
    inner: Arc<Mutex<TuiOutput>>,
    target: TargetStream,
}

impl TuiOutputStreamHandle {
    /// Copy current entries from an Output snapshot, forcing to this stream.
    pub async fn append(&self, output: &Output) {
        self.inner.lock().await.append_with_target(output, self.target).await;
    }

    /// Subscribe to live output, forcing entries to this stream.
    pub async fn subscribe(&self, output: &Output) {
        self.inner.lock().await.subscribe_with_target(output, self.target).await;
    }

    /// Write literal text to this stream.
    pub async fn write(&self, text: &str) {
        self.inner.lock().await.write_literal(text, self.target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::buffer::OutputBuffer;

    /// Helper: create an Output with some test entries.
    async fn make_test_output(entries: Vec<(&str, Option<Stream>)>) -> Output {
        let buf = Arc::new(Mutex::new(OutputBuffer::new(100)));
        {
            let mut b = buf.lock().await;
            for (i, (text, stream)) in entries.iter().enumerate() {
                let entry = LogEntry {
                    raw: text.to_string(),
                    parsed: crate::log::ParsedContent::PlainText,
                    source: "test".to_string(),
                    seq: i as u64,
                    received_at: chrono::Utc::now(),
                    timestamp: None,
                    level: None,
                    message: Some(text.to_string()),
                    fields: std::collections::HashMap::new(),
                    stream: *stream,
                };
                b.push(entry);
            }
        }
        Output(buf)
    }

    #[tokio::test]
    async fn test_append_preserves_stream_mapping() {
        let output = make_test_output(vec![
            ("stdout line", Some(Stream::Stdout)),
            ("stderr line", Some(Stream::Stderr)),
        ]).await;

        let mut tui_output = TuiOutput::new();
        tui_output.append(&output).await;

        let (stdout, stderr) = tui_output.flush().await;
        assert_eq!(stdout, "stdout line\n");
        assert_eq!(stderr, "stderr line\n");
    }

    #[tokio::test]
    async fn test_append_to_stderr_forces_all_to_stderr() {
        let output = make_test_output(vec![
            ("line 1", Some(Stream::Stdout)),
            ("line 2", Some(Stream::Stderr)),
        ]).await;

        let mut tui_output = TuiOutput::new();
        tui_output.stderr().append(&output).await;

        let (stdout, stderr) = tui_output.flush().await;
        assert!(stdout.is_empty());
        assert!(stderr.contains("line 1"));
        assert!(stderr.contains("line 2"));
    }

    #[tokio::test]
    async fn test_append_to_stdout_forces_all_to_stdout() {
        let output = make_test_output(vec![
            ("line 1", Some(Stream::Stdout)),
            ("line 2", Some(Stream::Stderr)),
        ]).await;

        let mut tui_output = TuiOutput::new();
        tui_output.stdout().append(&output).await;

        let (stdout, stderr) = tui_output.flush().await;
        assert!(stderr.is_empty());
        assert!(stdout.contains("line 1"));
        assert!(stdout.contains("line 2"));
    }

    #[tokio::test]
    async fn test_write_literal_text() {
        let mut tui_output = TuiOutput::new();
        tui_output.stderr().write("hello from task\n");
        tui_output.stdout().write("result data");

        let (stdout, stderr) = tui_output.flush().await;
        assert_eq!(stdout, "result data\n");
        assert_eq!(stderr, "hello from task\n");
    }

    #[tokio::test]
    async fn test_subscribe_captures_live_entries() {
        let buf = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let output = Output(buf.clone());

        let mut tui_output = TuiOutput::new();
        tui_output.subscribe(&output).await;

        // Push entries after subscribing
        {
            let mut b = buf.lock().await;
            let entry = LogEntry {
                raw: "live line".to_string(),
                parsed: crate::log::ParsedContent::PlainText,
                source: "test".to_string(),
                seq: 0,
                received_at: chrono::Utc::now(),
                timestamp: None,
                level: None,
                message: Some("live line".to_string()),
                fields: std::collections::HashMap::new(),
                stream: Some(Stream::Stdout),
            };
            b.push(entry);
        }

        // Small delay so broadcast can deliver
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (stdout, stderr) = tui_output.flush().await;
        assert_eq!(stdout, "live line\n");
        assert!(stderr.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_to_stderr_forces_stream() {
        let buf = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let output = Output(buf.clone());

        let mut tui_output = TuiOutput::new();
        tui_output.stderr().subscribe(&output).await;

        // Push a stdout entry — should be forced to stderr
        {
            let mut b = buf.lock().await;
            let entry = LogEntry {
                raw: "forced to stderr".to_string(),
                parsed: crate::log::ParsedContent::PlainText,
                source: "test".to_string(),
                seq: 0,
                received_at: chrono::Utc::now(),
                timestamp: None,
                level: None,
                message: Some("forced to stderr".to_string()),
                fields: std::collections::HashMap::new(),
                stream: Some(Stream::Stdout),
            };
            b.push(entry);
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (stdout, stderr) = tui_output.flush().await;
        assert!(stdout.is_empty());
        assert_eq!(stderr, "forced to stderr\n");
    }

    #[tokio::test]
    async fn test_flush_is_idempotent() {
        let mut tui_output = TuiOutput::new();
        tui_output.write_stdout("hello");

        let (stdout, stderr) = tui_output.flush().await;
        assert_eq!(stdout, "hello\n");
        assert!(stderr.is_empty());

        // Second flush should be empty
        let (stdout2, stderr2) = tui_output.flush().await;
        assert!(stdout2.is_empty());
        assert!(stderr2.is_empty());
    }

    #[tokio::test]
    async fn test_handle_append() {
        let output = make_test_output(vec![
            ("line a", Some(Stream::Stdout)),
        ]).await;

        let tui_output = Arc::new(Mutex::new(TuiOutput::new()));
        let handle = TuiOutputHandle::new(tui_output);

        handle.append(&output).await;
        let (stdout, stderr) = handle.flush().await;
        assert_eq!(stdout, "line a\n");
        assert!(stderr.is_empty());
    }

    #[tokio::test]
    async fn test_handle_stderr_append() {
        let output = make_test_output(vec![
            ("line b", Some(Stream::Stdout)),
        ]).await;

        let tui_output = Arc::new(Mutex::new(TuiOutput::new()));
        let handle = TuiOutputHandle::new(tui_output);

        handle.stderr().append(&output).await;
        let (stdout, stderr) = handle.flush().await;
        assert!(stdout.is_empty());
        assert_eq!(stderr, "line b\n");
    }

    #[tokio::test]
    async fn test_entries_without_stream_default_to_stderr() {
        let output = make_test_output(vec![
            ("tracing entry", None),
        ]).await;

        let mut tui_output = TuiOutput::new();
        tui_output.append(&output).await;

        let (stdout, stderr) = tui_output.flush().await;
        assert!(stdout.is_empty());
        assert_eq!(stderr, "tracing entry\n");
    }
}
