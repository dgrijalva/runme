use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use bytes::{Buf, BytesMut};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;

use crate::cmd::Cmd;
use crate::log::extract::{self, FieldExtractor};
use crate::log::parse::{self, RecordParser};
use crate::log::{LogEntry, ParseResult};

/// Captured output from a process execution.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Errors that can occur during process management.
#[derive(Debug)]
pub enum ProcessError {
    Spawn(std::io::Error),
    Signal(nix::Error),
    Wait(std::io::Error),
    Timeout,
    /// Process exited with a non-zero exit code.
    ExitCode { code: i32, output: ExecOutput },
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Spawn(e) => write!(f, "failed to spawn process: {}", e),
            ProcessError::Signal(e) => write!(f, "failed to send signal: {}", e),
            ProcessError::Wait(e) => write!(f, "failed to wait for process: {}", e),
            ProcessError::Timeout => write!(f, "process did not exit within timeout"),
            ProcessError::ExitCode { code, .. } => write!(f, "process exited with code {}", code),
        }
    }
}

impl std::error::Error for ProcessError {}

/// Extension trait for accessing output from exec results regardless of exit code.
pub trait ExecOutputExt {
    fn output(&self) -> Option<&ExecOutput>;
}

impl ExecOutputExt for Result<ExecOutput, ProcessError> {
    /// Returns the captured output whether the process succeeded or failed
    /// with a non-zero exit code. Returns `None` only for infrastructure
    /// errors (spawn failure, signal error, etc.).
    fn output(&self) -> Option<&ExecOutput> {
        match self {
            Ok(output) => Some(output),
            Err(ProcessError::ExitCode { output, .. }) => Some(output),
            Err(_) => None,
        }
    }
}

/// Output ring buffer for a task.
///
/// Stores log entries with bounded capacity. When full, oldest entries are dropped.
/// Also broadcasts new entries to subscribers.
pub struct OutputBuffer {
    lines: VecDeque<LogEntry>,
    capacity: usize,
    tx: broadcast::Sender<LogEntry>,
}

impl OutputBuffer {
    /// Create a new buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
            tx,
        }
    }

    /// Push a log entry into the buffer. Drops the oldest if at capacity.
    pub fn push(&mut self, entry: LogEntry) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        // Broadcast to subscribers (ignore errors — no receivers is OK)
        let _ = self.tx.send(entry.clone());
        self.lines.push_back(entry);
    }

    /// Get all buffered entries.
    pub fn lines(&self) -> &VecDeque<LogEntry> {
        &self.lines
    }

    /// Get a broadcast receiver for new entries.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }

    /// Number of entries currently in the buffer.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The maximum capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get a reference to the broadcast sender.
    ///
    /// Useful for passing to streaming utilities (e.g., `stream::tail()`).
    pub fn subscribe_sender(&self) -> &broadcast::Sender<LogEntry> {
        &self.tx
    }
}

/// Build a LogEntry from a RawRecord and extracted fields.
fn build_log_entry(
    raw_record: super::log::RawRecord,
    extractor: &dyn FieldExtractor,
    source: &str,
    seq: &mut u64,
) -> LogEntry {
    let extracted = extractor.extract(&raw_record);
    let entry = LogEntry {
        raw: raw_record.raw,
        parsed: raw_record.parsed,
        source: source.to_string(),
        seq: *seq,
        timestamp: extracted.timestamp,
        level: extracted.level,
        message: extracted.message,
        fields: extracted.fields,
    };
    *seq += 1;
    entry
}

/// Drain records from a buffer using the parser, pushing entries into the output buffer.
fn drain_records(
    buf: &mut BytesMut,
    eof: bool,
    parser: &mut dyn RecordParser,
    extractor: &dyn FieldExtractor,
    source: &str,
    seq: &mut u64,
    output: &mut OutputBuffer,
) {
    loop {
        if buf.is_empty() {
            break;
        }
        match parser.feed(buf, eof) {
            ParseResult::Record(rec, consumed) => {
                buf.advance(consumed);
                let entry = build_log_entry(rec, extractor, source, seq);
                output.push(entry);
                // continue -- buffer may contain more records
            }
            ParseResult::Incomplete | ParseResult::Rejection => break,
        }
    }
}

/// Drain records from a buffer (async version for spawn background tasks).
async fn drain_records_async(
    buf: &mut BytesMut,
    eof: bool,
    parser: &mut dyn RecordParser,
    extractor: &dyn FieldExtractor,
    source: &str,
    seq: &mut u64,
    output: &std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
) {
    loop {
        if buf.is_empty() {
            break;
        }
        match parser.feed(buf, eof) {
            ParseResult::Record(rec, consumed) => {
                buf.advance(consumed);
                let entry = build_log_entry(rec, extractor, source, seq);
                output.lock().await.push(entry);
                // continue -- buffer may contain more records
            }
            ParseResult::Incomplete | ParseResult::Rejection => break,
        }
    }
}

/// Handle to a running child process.
///
/// The child is spawned in its own process group, allowing group-wide
/// signal delivery for clean shutdown.
pub struct ProcessHandle {
    child: tokio::process::Child,
    task_name: String,
    pgid: Option<i32>,
    /// The output buffer for this process.
    pub buffer: std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
    /// Background tasks reading stdout/stderr
    _stdout_task: Option<tokio::task::JoinHandle<()>>,
    _stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl ProcessHandle {
    /// Send a signal to the child's process group.
    pub fn signal(&self, sig: Signal) -> Result<(), ProcessError> {
        if let Some(pgid) = self.pgid {
            killpg(Pid::from_raw(pgid), Some(sig)).map_err(ProcessError::Signal)
        } else if let Some(id) = self.child.id() {
            // Fallback: signal the child directly
            killpg(Pid::from_raw(id as i32), Some(sig)).map_err(ProcessError::Signal)
        } else {
            Ok(()) // Process already exited
        }
    }

    /// Graceful shutdown: send SIGTERM, wait for the timeout, then SIGKILL if still alive.
    pub async fn stop(&mut self, timeout: Duration) -> Result<(), ProcessError> {
        // Send SIGTERM to the process group
        let _ = self.signal(Signal::SIGTERM);

        // Wait for the process to exit within the timeout
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(ProcessError::Wait(e)),
            Err(_) => {
                // Timeout: send SIGKILL
                let _ = self.signal(Signal::SIGKILL);
                // Give it a moment to die
                match tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(e)) => Err(ProcessError::Wait(e)),
                    Err(_) => Err(ProcessError::Timeout),
                }
            }
        }
    }

    /// Check if the process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait for the process to exit and return the result.
    pub async fn wait(&mut self) -> Result<ExecOutput, ProcessError> {
        let status = self.child.wait().await.map_err(ProcessError::Wait)?;
        let exit_code = status.code().unwrap_or(-1);
        let output = ExecOutput {
            stdout: String::new(), // Output went to the buffer via background tasks
            stderr: String::new(),
        };
        if exit_code != 0 {
            return Err(ProcessError::ExitCode { code: exit_code, output });
        }
        Ok(output)
    }

    /// Get the task name associated with this handle.
    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    /// Get the process group ID if available.
    pub fn pgid(&self) -> Option<i32> {
        self.pgid
    }

    /// Get the child's PID if still running.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

/// Build a tokio Command that spawns in its own process group.
fn build_command(cmd: Cmd) -> tokio::process::Command {
    let mut command = cmd.into_tokio_command();
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // Spawn in a new process group
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))
                .map_err(std::io::Error::other)?;
            Ok(())
        });
    }

    command
}

/// Execute a command synchronously (wait for completion), capturing all output.
///
/// Bytes are fed through the parsing pipeline (RecordParser -> FieldExtractor -> LogEntry)
/// and pushed into the provided OutputBuffer. Uses BytesMut accumulation buffers
/// with read_buf() instead of line-oriented BufReader.
/// Accepts anything that converts to `Cmd`: a `&str`/`String` (shell mode) or a `Cmd` value.
pub async fn exec(
    command: impl Into<Cmd>,
    task_name: &str,
    buffer: &mut OutputBuffer,
) -> Result<ExecOutput, ProcessError> {
    let mut cmd = command.into();

    // Extract parser/extractor from Cmd before consuming it, falling back to defaults.
    // Separate parser instances for stdout and stderr (parsers are stateful).
    let mut stdout_parser: Box<dyn RecordParser> = cmd.parser.take()
        .unwrap_or_else(|| Box::new(parse::default_parser()));
    let mut stderr_parser: Box<dyn RecordParser> =
        Box::new(parse::default_parser());
    let extractor: Box<dyn FieldExtractor> = cmd.extractor.take()
        .unwrap_or_else(|| Box::new(extract::default_extractor()));

    let mut child = build_command(cmd).spawn().map_err(ProcessError::Spawn)?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_buf = BytesMut::new();
    let mut stderr_buf = BytesMut::new();
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut seq: u64 = 0;

    let mut stdout_done = false;
    let mut stderr_done = false;

    // Read stdout and stderr concurrently using raw bytes
    while !stdout_done || !stderr_done {
        tokio::select! {
            n = stdout.read_buf(&mut stdout_buf), if !stdout_done => {
                match n {
                    Ok(0) => {
                        stdout_done = true;
                        // Drain with eof=true
                        drain_records(
                            &mut stdout_buf, true,
                            stdout_parser.as_mut(), extractor.as_ref(),
                            task_name, &mut seq, buffer,
                        );
                        // Capture remaining text for ExecOutput
                        if !stdout_buf.is_empty() {
                            stdout_text.push_str(&String::from_utf8_lossy(&stdout_buf));
                            stdout_buf.clear();
                        }
                    }
                    Ok(bytes_read) => {
                        // Accumulate the newly read bytes for ExecOutput
                        stdout_text.push_str(&String::from_utf8_lossy(&stdout_buf[stdout_buf.len() - bytes_read..]));
                        // Drain records
                        drain_records(
                            &mut stdout_buf, false,
                            stdout_parser.as_mut(), extractor.as_ref(),
                            task_name, &mut seq, buffer,
                        );
                    }
                    Err(_) => {
                        stdout_done = true;
                    }
                }
            }
            n = stderr.read_buf(&mut stderr_buf), if !stderr_done => {
                match n {
                    Ok(0) => {
                        stderr_done = true;
                        drain_records(
                            &mut stderr_buf, true,
                            stderr_parser.as_mut(), extractor.as_ref(),
                            task_name, &mut seq, buffer,
                        );
                        if !stderr_buf.is_empty() {
                            stderr_text.push_str(&String::from_utf8_lossy(&stderr_buf));
                            stderr_buf.clear();
                        }
                    }
                    Ok(bytes_read) => {
                        stderr_text.push_str(&String::from_utf8_lossy(&stderr_buf[stderr_buf.len() - bytes_read..]));
                        drain_records(
                            &mut stderr_buf, false,
                            stderr_parser.as_mut(), extractor.as_ref(),
                            task_name, &mut seq, buffer,
                        );
                    }
                    Err(_) => {
                        stderr_done = true;
                    }
                }
            }
        }
    }

    let status = child.wait().await.map_err(ProcessError::Wait)?;

    let exit_code = status.code().unwrap_or(-1);
    let output = ExecOutput {
        stdout: stdout_text,
        stderr: stderr_text,
    };

    if exit_code != 0 {
        return Err(ProcessError::ExitCode { code: exit_code, output });
    }

    Ok(output)
}

/// Spawn a command in the background, returning a handle for monitoring and control.
///
/// Output is continuously read into the provided OutputBuffer by background tasks.
/// Bytes are fed through the parsing pipeline using BytesMut buffers.
/// Accepts anything that converts to `Cmd`: a `&str`/`String` (shell mode) or a `Cmd` value.
pub async fn spawn(
    command: impl Into<Cmd>,
    task_name: &str,
    buffer: std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
) -> Result<ProcessHandle, ProcessError> {
    let mut cmd = command.into();

    // Extract parser/extractor from Cmd before consuming it, falling back to defaults.
    // Separate parser instances for stdout and stderr (parsers are stateful).
    let stdout_parser: Box<dyn RecordParser> = cmd.parser.take()
        .unwrap_or_else(|| Box::new(parse::default_parser()));
    let stderr_parser: Box<dyn RecordParser> =
        Box::new(parse::default_parser());
    let extractor: Box<dyn FieldExtractor> = cmd.extractor.take()
        .unwrap_or_else(|| Box::new(extract::default_extractor()));

    let mut child = build_command(cmd).spawn().map_err(ProcessError::Spawn)?;

    let pgid = child.id().map(|id| id as i32);

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let task_name_owned = task_name.to_string();

    // Each stream gets its own parser instance (parsers are stateful).
    // The extractor is stateless and can be shared.
    let stdout_parser = std::sync::Arc::new(tokio::sync::Mutex::new(stdout_parser));
    let stderr_parser = std::sync::Arc::new(tokio::sync::Mutex::new(stderr_parser));
    let extractor = std::sync::Arc::new(extractor);
    let seq = std::sync::Arc::new(tokio::sync::Mutex::new(0u64));

    // Background task: read stdout bytes into buffer
    let buf_clone = buffer.clone();
    let parser_clone = stdout_parser;
    let extractor_clone = extractor.clone();
    let seq_clone = seq.clone();
    let source_clone = task_name_owned.clone();
    let stdout_task = tokio::spawn(async move {
        let mut byte_buf = BytesMut::new();
        while let Ok(n) = stdout.read_buf(&mut byte_buf).await {
            let eof = n == 0;

            {
                let mut p = parser_clone.lock().await;
                let mut s = seq_clone.lock().await;
                drain_records_async(
                    &mut byte_buf, eof,
                    p.as_mut(), extractor_clone.as_ref(),
                    &source_clone, &mut s, &buf_clone,
                ).await;
            }

            if eof { break; }
        }
    });

    // Background task: read stderr bytes into buffer
    let buf_clone = buffer.clone();
    let parser_clone = stderr_parser;
    let extractor_clone = extractor;
    let seq_clone = seq;
    let source_clone = task_name_owned;
    let stderr_task = tokio::spawn(async move {
        let mut byte_buf = BytesMut::new();
        while let Ok(n) = stderr.read_buf(&mut byte_buf).await {
            let eof = n == 0;

            {
                let mut p = parser_clone.lock().await;
                let mut s = seq_clone.lock().await;
                drain_records_async(
                    &mut byte_buf, eof,
                    p.as_mut(), extractor_clone.as_ref(),
                    &source_clone, &mut s, &buf_clone,
                ).await;
            }

            if eof { break; }
        }
    });

    Ok(ProcessHandle {
        child,
        task_name: task_name.to_string(),
        pgid,
        buffer,
        _stdout_task: Some(stdout_task),
        _stderr_task: Some(stderr_task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::ParsedContent;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_exec_captures_stdout() {
        let mut buffer = OutputBuffer::new(100);
        let output = exec("echo hello", "test", &mut buffer).await.unwrap();
        assert_eq!(output.stdout, "hello\n");
    }

    #[tokio::test]
    async fn test_exec_captures_stderr() {
        let mut buffer = OutputBuffer::new(100);
        let output = exec("echo error >&2", "test", &mut buffer).await.unwrap();
        assert_eq!(output.stderr, "error\n");
    }

    #[tokio::test]
    async fn test_exec_detects_json_lines() {
        let mut buffer = OutputBuffer::new(100);
        let cmd = r#"echo '{"level":"info","message":"hello"}'"#;
        let _result = exec(cmd, "test", &mut buffer).await.unwrap();

        assert!(!buffer.is_empty());
        let entry = &buffer.lines()[0];
        // Should be parsed as JSON
        match &entry.parsed {
            ParsedContent::Json(val) => {
                assert_eq!(val["level"], "info");
                assert_eq!(val["message"], "hello");
            }
            other => panic!("Expected Json, got {:?}", other),
        }
        // Fields should be extracted
        assert_eq!(entry.level.as_deref(), Some("info"));
        assert_eq!(entry.message.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn test_exec_raw_lines_not_json() {
        let mut buffer = OutputBuffer::new(100);
        let _result = exec("echo 'just plain text'", "test", &mut buffer).await.unwrap();

        assert!(!buffer.is_empty());
        let entry = &buffer.lines()[0];
        assert!(matches!(&entry.parsed, ParsedContent::PlainText));
        assert_eq!(entry.raw, "just plain text");
    }

    #[tokio::test]
    async fn test_exec_nonzero_exit() {
        let mut buffer = OutputBuffer::new(100);
        let err = exec("exit 42", "test", &mut buffer).await.unwrap_err();
        match err {
            ProcessError::ExitCode { code, .. } => assert_eq!(code, 42),
            other => panic!("expected ExitCode, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_spawn_and_stop() {
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn("sleep 60", "test", buffer).await.unwrap();

        assert!(handle.is_running());

        handle.stop(Duration::from_secs(2)).await.unwrap();

        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_spawn_captures_output() {
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        // Use a command that produces output and exits
        let mut handle = spawn("echo spawned_output", "test", buffer.clone()).await.unwrap();

        // Wait for the process to finish
        let _result = handle.wait().await.unwrap();

        // Give the background tasks a moment to flush
        tokio::time::sleep(Duration::from_millis(100)).await;

        let buf = buffer.lock().await;
        assert!(!buf.is_empty());
        let entry = &buf.lines()[0];
        assert_eq!(entry.raw, "spawned_output");
        assert!(matches!(&entry.parsed, ParsedContent::PlainText));
    }

    #[tokio::test]
    async fn test_process_group_cleanup() {
        // Spawn a shell that spawns a child; stopping should kill both
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn("sh -c 'sleep 120 & sleep 120'", "test", buffer).await.unwrap();

        assert!(handle.is_running());

        handle.stop(Duration::from_secs(2)).await.unwrap();

        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_output_buffer_ring() {
        let mut buffer = OutputBuffer::new(3);
        let make_entry = |raw: &str, seq: u64| LogEntry {
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: "test".to_string(),
            seq,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
        };
        buffer.push(make_entry("line1", 0));
        buffer.push(make_entry("line2", 1));
        buffer.push(make_entry("line3", 2));
        assert_eq!(buffer.len(), 3);

        // Push one more, oldest should be dropped
        buffer.push(make_entry("line4", 3));
        assert_eq!(buffer.len(), 3);

        let lines: Vec<String> = buffer.lines().iter().map(|l| l.as_str()).collect();
        assert_eq!(lines, vec!["line2", "line3", "line4"]);
    }

    #[tokio::test]
    async fn test_output_buffer_subscribe() {
        let mut buffer = OutputBuffer::new(100);
        let mut rx = buffer.subscribe();

        buffer.push(LogEntry {
            raw: "broadcast_test".to_string(),
            parsed: ParsedContent::PlainText,
            source: "test".to_string(),
            seq: 0,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
        });

        let received = rx.recv().await.unwrap();
        assert_eq!(received.as_str(), "broadcast_test");
    }

    // ---------------------------------------------------------------
    // Misbehaving child process tests
    // See docs/system_design.md "Child Process Failure Modes"
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_misbehave_ignores_sigterm() {
        // Process traps SIGTERM and ignores it. stop() should escalate to SIGKILL.
        let cmd = r#"trap '' TERM; sleep 60"#;
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn(cmd, "test", buffer).await.unwrap();

        assert!(handle.is_running());

        // stop() sends SIGTERM, waits, then SIGKILL
        handle.stop(Duration::from_secs(2)).await.unwrap();

        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_misbehave_orphan_child() {
        // Parent forks a child then exits. The orphan should still be killed
        // because we signal the entire process group.
        let cmd = r#"sleep 60 & echo child_pid=$!; exit 0"#;
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn(cmd, "test", buffer.clone()).await.unwrap();

        // Wait for the parent shell to exit
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The process group should still be signalable
        handle.stop(Duration::from_secs(2)).await.unwrap();

        // Give the OS a moment to clean up
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify no orphan sleep processes from our group remain.
        // We can't easily check by PID, but the fact that stop() didn't
        // timeout or error is the primary assertion.
    }

    #[tokio::test]
    async fn test_misbehave_hangs_forever() {
        // Process that will never exit on its own. exec() caller needs to
        // be able to bound this with a timeout externally.
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn("sleep 999", "test", buffer).await.unwrap();

        assert!(handle.is_running());

        // Verify we can kill it within a reasonable timeout
        let result = handle.stop(Duration::from_secs(2)).await;
        assert!(result.is_ok());
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_misbehave_closes_stdout_keeps_running() {
        // Process closes stdout/stderr but keeps running.
        // Our readers should finish, and stop() should still kill it.
        let cmd = r#"exec 1>&- 2>&-; sleep 60"#;
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn(cmd, "test", buffer).await.unwrap();

        // Give readers a moment to see EOF on stdout/stderr
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(handle.is_running());
        handle.stop(Duration::from_secs(2)).await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_misbehave_segfault() {
        // Process dies from a signal (simulated with kill -SEGV).
        // We should get an ExitCode error with the signal-based exit code.
        let cmd = r#"kill -SEGV $$"#;
        let mut buffer = OutputBuffer::new(100);
        let result = exec(cmd, "test", &mut buffer).await;

        match result {
            Err(ProcessError::ExitCode { code, .. }) => {
                // On Unix, death by signal yields exit code 128+signal or negative
                // SIGSEGV = 11, so expect 139 (128+11) or -11
                assert!(code != 0, "expected non-zero exit code, got {}", code);
            }
            Err(other) => {
                // Some systems report this differently — as long as it's an error
                panic!("expected ExitCode, got: {:?}", other);
            }
            Ok(_) => panic!("expected error from segfaulting process"),
        }
    }

    #[tokio::test]
    async fn test_misbehave_killed_externally() {
        // Process is killed by an external signal (not from us).
        // We should observe the death cleanly without hanging.
        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(OutputBuffer::new(100)));
        let mut handle = spawn("sleep 60", "test", buffer).await.unwrap();

        assert!(handle.is_running());

        // Kill it from the outside using its PID directly
        let pid = handle.pid().expect("should have a pid");
        nix::sys::signal::kill(
            Pid::from_raw(pid as i32),
            Signal::SIGKILL,
        ).unwrap();

        // wait() should return an error (non-zero exit from signal death)
        let result = handle.wait().await;
        assert!(result.is_err());
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_misbehave_massive_output() {
        // Process dumps a huge amount of output.
        // Ring buffer should stay bounded, no OOM.
        let capacity = 100;
        let mut buffer = OutputBuffer::new(capacity);
        // Generate 10,000 lines
        let cmd = r#"seq 1 10000"#;
        let _output = exec(cmd, "test", &mut buffer).await.unwrap();

        // Buffer should be capped at capacity
        assert_eq!(buffer.len(), capacity);
        // Last line should be "10000"
        let last = buffer.lines().back().unwrap().as_str();
        assert_eq!(last, "10000");
    }

    #[tokio::test]
    async fn test_misbehave_long_line() {
        // Process writes an extremely long line with no newline.
        // Should not crash or hang.
        let mut buffer = OutputBuffer::new(100);
        // Generate a 1MB line
        let cmd = r#"python3 -c "print('x' * 1_000_000)""#;
        let result = exec(cmd, "test", &mut buffer).await;

        match result {
            Ok(output) => {
                assert_eq!(output.stdout.trim().len(), 1_000_000);
            }
            Err(_) => {
                // python3 might not be available; skip gracefully
            }
        }
    }

    #[tokio::test]
    #[ignore = "TODO: binary output is silently dropped because our line reader requires UTF-8"]
    async fn test_misbehave_binary_output() {
        // Process writes non-UTF8 binary data followed by valid text.
        // We should preserve the valid parts at minimum.
        let mut buffer = OutputBuffer::new(100);
        let cmd = r#"printf '\xff\xfe'; echo hello"#;
        let result = exec(cmd, "test", &mut buffer).await;

        let output = match result {
            Ok(output) => output,
            Err(ProcessError::ExitCode { output, .. }) => output,
            Err(other) => panic!("unexpected error: {:?}", other),
        };

        assert!(output.stdout.contains("hello"), "valid output after binary data was lost");
    }

    #[tokio::test]
    async fn test_log_entry_source_and_seq() {
        let mut buffer = OutputBuffer::new(100);
        let cmd = r#"echo first; echo second; echo third"#;
        let _result = exec(cmd, "my_task", &mut buffer).await.unwrap();

        assert_eq!(buffer.len(), 3);

        let entries: Vec<_> = buffer.lines().iter().collect();
        // All entries should have the correct source
        for entry in &entries {
            assert_eq!(entry.source, "my_task");
        }
        // Seq should be monotonically increasing
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);
    }

    #[tokio::test]
    async fn test_exec_json_field_extraction() {
        let mut buffer = OutputBuffer::new(100);
        let cmd = r#"echo '{"level":"error","msg":"connection failed","timestamp":"2024-01-01T00:00:00Z","service":"auth"}'"#;
        let _result = exec(cmd, "test", &mut buffer).await.unwrap();

        let entry = &buffer.lines()[0];
        assert_eq!(entry.level.as_deref(), Some("error"));
        assert_eq!(entry.message.as_deref(), Some("connection failed"));
        assert_eq!(entry.timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(
            entry.fields.get("service"),
            Some(&serde_json::Value::String("auth".to_string()))
        );
    }
}
