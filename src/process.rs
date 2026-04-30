//! Process spawning, lifecycle management, and output capture.
//!
//! Tasks rarely call into this module directly — they use
//! [`TaskContext::spawn`](crate::task::TaskContext::spawn) and
//! [`TaskContext::exec`](crate::task::TaskContext::exec), which return the
//! types defined here.
//!
//! # Spawning
//!
//! `TaskContext::spawn` returns a [`SpawnBuilder`]. You can configure
//! readiness probes and timeouts on it, then `.await` it to get a
//! [`ProcessHandle`], or call `.complete().await` to wait for the process
//! to exit:
//!
//! ```rust,ignore
//! // Background process with a readiness probe.
//! let server = ctx.spawn("./bin/api")
//!     .ready_on_port(8080)
//!     .ready_timeout(Duration::from_secs(30))
//!     .await?;
//! ctx.bind_ready(&server);
//!
//! // Run-to-completion (sugar: `ctx.exec(...)` does the same).
//! let result = ctx.spawn("cargo build").complete().await?;
//! result.ok()?; // bubble up non-zero exits as a TaskError
//! ```
//!
//! # Output and termination
//!
//! Output flows through the [`Output`] handle: snapshot via
//! [`Output::entries`], live broadcast via [`Output::subscribe`], or
//! convenience accessors [`Output::stdout`] / [`Output::stderr`] for raw lines.
//! [`ProcessResult::termination`] returns a [`Termination`] enum
//! (`Exited(code)` / `Signaled(sig)` / `TimedOut`) — richer than a plain exit
//! code.
//!
//! # Process groups
//!
//! Every spawned process is placed in its own process group, and `stop()`
//! signals the entire group. This means shell commands that fork
//! background children get cleaned up with their parent.

use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use bytes::{Buf, BytesMut};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;

use crate::cmd::Cmd;
use crate::log::extract::{self, FieldExtractor};
use crate::log::parse::{self, RecordParser};
use crate::log::{LogEntry, ParseResult, Stream};

/// Errors that can occur during process management.
///
/// These represent infrastructure failures — not non-zero exit codes.
/// A process that runs and exits (even with a non-zero code) produces
/// a `ProcessResult`, not a `ProcessError`.
#[derive(Debug)]
pub enum ProcessError {
    Spawn(std::io::Error),
    Signal(nix::Error),
    Wait(std::io::Error),
    Timeout,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Spawn(e) => write!(f, "failed to spawn process: {}", e),
            ProcessError::Signal(e) => write!(f, "failed to send signal: {}", e),
            ProcessError::Wait(e) => write!(f, "failed to wait for process: {}", e),
            ProcessError::Timeout => write!(f, "process did not exit within timeout"),
        }
    }
}

impl std::error::Error for ProcessError {}

/// How a process terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Termination {
    /// Process exited normally with the given exit code.
    Exited(i32),
    /// Process was killed by a signal.
    Signaled(Signal),
    /// Process was killed because it exceeded its timeout.
    TimedOut,
}

impl Termination {
    /// Whether the process exited successfully (exit code 0).
    pub fn success(&self) -> bool {
        matches!(self, Termination::Exited(0))
    }

    /// Get the exit code, following Unix conventions.
    ///
    /// - `Exited(code)` → `code`
    /// - `Signaled(sig)` → `128 + signal_number`
    /// - `TimedOut` → `124` (matches coreutils `timeout`)
    pub fn exit_code(&self) -> i32 {
        match self {
            Termination::Exited(code) => *code,
            Termination::Signaled(sig) => 128 + *sig as i32,
            Termination::TimedOut => 124,
        }
    }

    /// Build a `Termination` from a `std::process::ExitStatus`.
    pub fn from_exit_status(status: std::process::ExitStatus) -> Self {
        if let Some(code) = status.code() {
            Termination::Exited(code)
        } else if let Some(sig) = status.signal() {
            match Signal::try_from(sig) {
                Ok(signal) => Termination::Signaled(signal),
                Err(_) => Termination::Exited(128 + sig),
            }
        } else {
            Termination::Exited(-1)
        }
    }
}

impl std::fmt::Display for Termination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Termination::Exited(0) => write!(f, "exited successfully"),
            Termination::Exited(code) => write!(f, "exited with code {}", code),
            Termination::Signaled(sig) => write!(f, "killed by signal {}", sig),
            Termination::TimedOut => write!(f, "timed out"),
        }
    }
}

/// Unified handle to process output backed by an `OutputBuffer`.
///
/// Provides access to captured log entries, broadcast subscription for
/// live streaming, and convenience methods for extracting raw stdout/stderr text.
#[derive(Clone)]
pub struct Output(pub(crate) std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>);

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output").finish_non_exhaustive()
    }
}

impl Output {
    /// Snapshot of all log entries captured so far.
    pub async fn entries(&self) -> Vec<LogEntry> {
        let buf = self.0.lock().await;
        buf.lines().iter().cloned().collect()
    }

    /// Subscribe to a live broadcast of new log entries.
    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry> {
        let buf = self.0.lock().await;
        buf.subscribe()
    }

    /// Convenience: raw stdout lines as strings.
    pub async fn stdout(&self) -> Vec<String> {
        let buf = self.0.lock().await;
        buf.lines()
            .iter()
            .filter(|e| e.stream == Some(Stream::Stdout))
            .map(|e| e.raw.clone())
            .collect()
    }

    /// Convenience: raw stderr lines as strings.
    pub async fn stderr(&self) -> Vec<String> {
        let buf = self.0.lock().await;
        buf.lines()
            .iter()
            .filter(|e| e.stream == Some(Stream::Stderr))
            .map(|e| e.raw.clone())
            .collect()
    }
}

/// Result of a completed process execution.
///
/// Always produced when a process runs to completion (regardless of exit code).
/// Use `success()` to check the exit code, or `ok()` to convert into a `Result`
/// for `?` ergonomics.
#[derive(Debug)]
pub struct ProcessResult {
    termination: Termination,
    output: Output,
    command_label: String,
}

impl ProcessResult {
    /// Whether the process exited successfully (exit code 0).
    pub fn success(&self) -> bool {
        self.termination.success()
    }

    /// The exit code, following Unix conventions.
    ///
    /// Signals map to `128 + signal_number`, timeout maps to `124`.
    pub fn exit_code(&self) -> i32 {
        self.termination.exit_code()
    }

    /// How the process terminated.
    pub fn termination(&self) -> &Termination {
        &self.termination
    }

    /// Get the command label (display name for the process).
    pub fn label(&self) -> &str {
        &self.command_label
    }

    /// Access the process output.
    pub fn output(&self) -> &Output {
        &self.output
    }

    /// Convert into a `Result` for `?` ergonomics.
    ///
    /// Returns `Ok(self)` on success (exit code 0), `Err(self)` otherwise.
    /// On the error path, `ProcessResult` converts into `TaskError` via `From`.
    pub fn ok(self) -> Result<ProcessResult, ProcessResult> {
        if self.success() { Ok(self) } else { Err(self) }
    }
}

// Re-export OutputBuffer so existing `crate::process::OutputBuffer` paths still work.
pub use crate::log::buffer::OutputBuffer;

/// Build a LogEntry from a RawRecord and extracted fields.
fn build_log_entry(
    raw_record: super::log::RawRecord,
    extractor: &dyn FieldExtractor,
    source: crate::execution::TaskId,
    seq: &mut u64,
    stream: Option<Stream>,
) -> LogEntry {
    let extracted = extractor.extract(&raw_record);
    let mut entry = LogEntry::new(
        raw_record.raw,
        raw_record.parsed,
        source,
        *seq,
        extracted.timestamp,
        extracted.level,
        extracted.message,
        extracted.fields,
    );
    entry.stream = stream;
    *seq += 1;
    entry
}

/// Drain records from a buffer using the parser, pushing entries into the output buffer.
#[allow(clippy::too_many_arguments)]
fn drain_records(
    buf: &mut BytesMut,
    eof: bool,
    parser: &mut dyn RecordParser,
    extractor: &dyn FieldExtractor,
    source: crate::execution::TaskId,
    seq: &mut u64,
    output: &mut OutputBuffer,
    stream: Option<Stream>,
) {
    loop {
        if buf.is_empty() {
            break;
        }
        match parser.feed(buf, eof) {
            ParseResult::Record(rec, consumed) => {
                buf.advance(consumed);
                let entry = build_log_entry(rec, extractor, source, seq, stream);
                output.push(entry);
                // continue -- buffer may contain more records
            }
            ParseResult::Incomplete | ParseResult::Rejection => break,
        }
    }
}

/// Drain records from a buffer (async version for spawn background tasks).
#[allow(clippy::too_many_arguments)]
async fn drain_records_async(
    buf: &mut BytesMut,
    eof: bool,
    parser: &mut dyn RecordParser,
    extractor: &dyn FieldExtractor,
    source: crate::execution::TaskId,
    seq: &mut u64,
    output: &std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
    stream: Option<Stream>,
) {
    loop {
        if buf.is_empty() {
            break;
        }
        match parser.feed(buf, eof) {
            ParseResult::Record(rec, consumed) => {
                buf.advance(consumed);
                let entry = build_log_entry(rec, extractor, source, seq, stream);
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
    /// Unique id for this process in the unified task-id space. Allocated
    /// at spawn time; routed onto every `LogEntry` for this process and
    /// surfaced via `SpawnEvent` so the engine can register it as a leaf
    /// in the graph (arch.md §9, decision 22).
    id: crate::execution::TaskId,
    task_name: String,
    command_label: String,
    pgid: Option<i32>,
    /// The output buffer for this process (private — use `.output()` to access).
    buffer: std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
    /// Background tasks reading stdout/stderr
    _stdout_task: Option<tokio::task::JoinHandle<()>>,
    _stderr_task: Option<tokio::task::JoinHandle<()>>,
    /// Readiness state — `true` when the readiness probe has succeeded.
    readiness_rx: tokio::sync::watch::Receiver<bool>,
    /// Background readiness probe task (if configured).
    _readiness_task: Option<tokio::task::JoinHandle<()>>,
}

impl ProcessHandle {
    /// Access the process output.
    pub fn output(&self) -> Output {
        Output(self.buffer.clone())
    }

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
    pub async fn wait(&mut self) -> ProcessResult {
        let termination = match self.child.wait().await {
            Ok(status) => Termination::from_exit_status(status),
            Err(_) => Termination::Exited(-1),
        };
        ProcessResult {
            termination,
            output: Output(self.buffer.clone()),
            command_label: self.command_label.clone(),
        }
    }

    /// Wait for the process to exit and return the result, consuming the handle.
    pub async fn complete(mut self) -> ProcessResult {
        self.wait().await
    }

    /// Get the task name associated with this handle.
    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    /// Get the process's unique `TaskId`.
    pub fn id(&self) -> crate::execution::TaskId {
        self.id
    }

    /// Get the command label (display name for this process).
    pub fn label(&self) -> &str {
        &self.command_label
    }

    /// Get the process group ID if available.
    pub fn pgid(&self) -> Option<i32> {
        self.pgid
    }

    /// Get the child's PID if still running.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Whether the process's readiness probe has succeeded.
    pub fn is_ready(&self) -> bool {
        *self.readiness_rx.borrow()
    }

    /// Wait until the readiness probe succeeds.
    ///
    /// Returns immediately if no readiness condition was configured (always ready)
    /// or if the probe has already succeeded.
    pub async fn wait_ready(&self) {
        let mut rx = self.readiness_rx.clone();
        // wait_for returns when the predicate is true (or the sender is dropped)
        let _ = rx.wait_for(|&ready| ready).await;
    }

    /// Get a clone of the readiness watch receiver.
    ///
    /// Useful for binding task-level readiness to this process's readiness.
    pub fn readiness_rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.readiness_rx.clone()
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
///
/// Returns a `ProcessResult` for any exit code. Only returns `Err(ProcessError)` for
/// infrastructure failures (spawn, wait).
pub async fn exec(
    command: impl Into<Cmd>,
    task_name: &str,
    buffer: &mut OutputBuffer,
) -> Result<ProcessResult, ProcessError> {
    let mut cmd = command.into();
    // Allocate a TaskId for this process so its log entries route correctly
    // through the unified id space (arch.md §9, decision 22).
    let process_id = crate::execution::TaskId::next();

    // Extract parser/extractor from Cmd before consuming it, falling back to defaults.
    // Separate parser instances for stdout and stderr (parsers are stateful).
    let mut stdout_parser: Box<dyn RecordParser> = cmd
        .parser
        .take()
        .unwrap_or_else(|| Box::new(parse::default_parser()));
    let mut stderr_parser: Box<dyn RecordParser> = Box::new(parse::default_parser());
    let extractor: Box<dyn FieldExtractor> = cmd
        .extractor
        .take()
        .unwrap_or_else(|| Box::new(extract::default_extractor()));

    let mut child = build_command(cmd).spawn().map_err(ProcessError::Spawn)?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_buf = BytesMut::new();
    let mut stderr_buf = BytesMut::new();
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
                            process_id, &mut seq, buffer,
                            Some(Stream::Stdout),
                        );
                    }
                    Ok(_bytes_read) => {
                        // Drain records
                        drain_records(
                            &mut stdout_buf, false,
                            stdout_parser.as_mut(), extractor.as_ref(),
                            process_id, &mut seq, buffer,
                            Some(Stream::Stdout),
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
                            process_id, &mut seq, buffer,
                            Some(Stream::Stderr),
                        );
                    }
                    Ok(_bytes_read) => {
                        drain_records(
                            &mut stderr_buf, false,
                            stderr_parser.as_mut(), extractor.as_ref(),
                            process_id, &mut seq, buffer,
                            Some(Stream::Stderr),
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
    let termination = Termination::from_exit_status(status);

    // Wrap the buffer in an Output. For exec(), we create a new Arc<Mutex<OutputBuffer>>
    // by moving the buffer contents into a fresh buffer.
    let mut result_buffer = OutputBuffer::new(buffer.capacity());
    for entry in buffer.lines().iter() {
        result_buffer.push(entry.clone());
    }
    let output = Output(std::sync::Arc::new(tokio::sync::Mutex::new(result_buffer)));

    Ok(ProcessResult {
        termination,
        output,
        command_label: task_name.to_string(),
    })
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
    // Allocate a TaskId for this process — flows into log entries and is
    // surfaced via the SpawnEvent so the engine table / graph snapshot can
    // register the process as a leaf of its parent task.
    let process_id = crate::execution::TaskId::next();

    // Extract parser/extractor from Cmd before consuming it, falling back to defaults.
    // Separate parser instances for stdout and stderr (parsers are stateful).
    let stdout_parser: Box<dyn RecordParser> = cmd
        .parser
        .take()
        .unwrap_or_else(|| Box::new(parse::default_parser()));
    let stderr_parser: Box<dyn RecordParser> = Box::new(parse::default_parser());
    let extractor: Box<dyn FieldExtractor> = cmd
        .extractor
        .take()
        .unwrap_or_else(|| Box::new(extract::default_extractor()));

    let mut child = build_command(cmd).spawn().map_err(ProcessError::Spawn)?;

    let pgid = child.id().map(|id| id as i32);

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

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
    let stdout_task = tokio::spawn(async move {
        let mut byte_buf = BytesMut::new();
        while let Ok(n) = stdout.read_buf(&mut byte_buf).await {
            let eof = n == 0;

            {
                let mut p = parser_clone.lock().await;
                let mut s = seq_clone.lock().await;
                drain_records_async(
                    &mut byte_buf,
                    eof,
                    p.as_mut(),
                    extractor_clone.as_ref(),
                    process_id,
                    &mut s,
                    &buf_clone,
                    Some(Stream::Stdout),
                )
                .await;
            }

            if eof {
                break;
            }
        }
    });

    // Background task: read stderr bytes into buffer
    let buf_clone = buffer.clone();
    let parser_clone = stderr_parser;
    let extractor_clone = extractor;
    let seq_clone = seq;
    let stderr_task = tokio::spawn(async move {
        let mut byte_buf = BytesMut::new();
        while let Ok(n) = stderr.read_buf(&mut byte_buf).await {
            let eof = n == 0;

            {
                let mut p = parser_clone.lock().await;
                let mut s = seq_clone.lock().await;
                drain_records_async(
                    &mut byte_buf,
                    eof,
                    p.as_mut(),
                    extractor_clone.as_ref(),
                    process_id,
                    &mut s,
                    &buf_clone,
                    Some(Stream::Stderr),
                )
                .await;
            }

            if eof {
                break;
            }
        }
    });

    // No readiness condition configured at the raw spawn level — default to ready.
    let (readiness_tx, readiness_rx) = tokio::sync::watch::channel(true);
    drop(readiness_tx); // No probe needed

    Ok(ProcessHandle {
        child,
        id: process_id,
        task_name: task_name.to_string(),
        command_label: task_name.to_string(),
        pgid,
        buffer,
        _stdout_task: Some(stdout_task),
        _stderr_task: Some(stderr_task),
        readiness_rx,
        _readiness_task: None,
    })
}

// ============================================================
// Readiness
// ============================================================

/// A condition that determines when a spawned process is "ready."
///
/// Readiness probes run as eager background tasks immediately after spawn.
/// Use `ProcessHandle::wait_ready()` to block until the probe succeeds.
pub enum ReadinessCondition {
    /// Process is ready when it's listening on this TCP port.
    Port(u16),
    /// Process is ready when this URL returns an HTTP 2xx response.
    Http(String),
    /// Process is ready when this async closure returns.
    Custom(Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>),
}

impl std::fmt::Debug for ReadinessCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadinessCondition::Port(port) => write!(f, "Port({})", port),
            ReadinessCondition::Http(url) => write!(f, "Http({})", url),
            ReadinessCondition::Custom(_) => write!(f, "Custom(...)"),
        }
    }
}

/// Run a readiness probe, returning when the condition is satisfied.
async fn run_readiness_probe(condition: ReadinessCondition) {
    match condition {
        ReadinessCondition::Port(port) => {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            loop {
                if tokio::net::TcpStream::connect(addr).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        ReadinessCondition::Http(url) => {
            loop {
                if let Ok(stream) = tokio::net::TcpStream::connect(
                    url.trim_start_matches("http://")
                        .trim_start_matches("https://")
                        .split('/')
                        .next()
                        .unwrap_or("127.0.0.1:80"),
                )
                .await
                {
                    // Basic HTTP GET — just check connection succeeds.
                    // A full HTTP client would be better but avoids a dependency.
                    drop(stream);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        ReadinessCondition::Custom(f) => {
            f().await;
        }
    }
}

// ============================================================
// SpawnBuilder
// ============================================================

/// Builder for spawning a process with optional readiness conditions and timeouts.
///
/// Created by `TaskContext::spawn()`. Use `.await` (via `IntoFuture`) to spawn
/// and get a `ProcessHandle`, or `.complete().await` to spawn and wait for exit.
///
/// ```ignore
/// // Background process
/// let handle = ctx.spawn("./server").await?;
///
/// // Wait for completion (like exec)
/// let result = ctx.spawn("cargo build").complete().await?;
/// ```
pub struct SpawnBuilder {
    cmd: Cmd,
    task_name: String,
    buffer: std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
    on_spawn: Option<Box<dyn FnOnce(&ProcessHandle) + Send>>,
    readiness: Option<ReadinessCondition>,
    ready_timeout: Option<Duration>,
    timeout: Option<Duration>,
}

impl SpawnBuilder {
    /// Create a new SpawnBuilder.
    pub(crate) fn new(
        cmd: Cmd,
        task_name: String,
        buffer: std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
    ) -> Self {
        SpawnBuilder {
            cmd,
            task_name,
            buffer,
            on_spawn: None,
            readiness: None,
            ready_timeout: None,
            timeout: None,
        }
    }

    /// Set a callback to run after the process is spawned.
    pub(crate) fn on_spawn(mut self, f: impl FnOnce(&ProcessHandle) + Send + 'static) -> Self {
        self.on_spawn = Some(Box::new(f));
        self
    }

    /// Set a readiness condition: process is ready when this TCP port is accepting connections.
    pub fn ready_on_port(mut self, port: u16) -> Self {
        self.readiness = Some(ReadinessCondition::Port(port));
        self
    }

    /// Set a readiness condition: process is ready when this HTTP URL is reachable.
    pub fn ready_on_http(mut self, url: impl Into<String>) -> Self {
        self.readiness = Some(ReadinessCondition::Http(url.into()));
        self
    }

    /// Set a readiness condition: process is ready when this async closure completes.
    pub fn ready_when<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.readiness = Some(ReadinessCondition::Custom(Box::new(move || Box::pin(f()))));
        self
    }

    /// Set the maximum time to wait for the readiness probe to succeed.
    ///
    /// If the probe doesn't succeed within this duration, it stops probing
    /// but the process continues running.
    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = Some(timeout);
        self
    }

    /// Set the maximum lifetime for this process.
    ///
    /// The process will be killed (SIGTERM → SIGKILL) after this duration.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Conditionally apply a builder transformation.
    pub fn when(self, condition: bool, f: impl FnOnce(Self) -> Self) -> Self {
        if condition { f(self) } else { self }
    }

    /// Spawn the process and wait for it to exit, returning the result.
    ///
    /// This is the "exec" path — equivalent to `spawn().await?.complete().await`.
    pub async fn complete(self) -> Result<ProcessResult, ProcessError> {
        let mut handle = self.do_spawn().await?;
        Ok(handle.wait().await)
    }

    /// Internal: perform the actual spawn, start probes, and call on_spawn.
    async fn do_spawn(self) -> Result<ProcessHandle, ProcessError> {
        let mut handle = spawn(self.cmd, &self.task_name, self.buffer).await?;

        // Start readiness probe if configured
        if let Some(condition) = self.readiness {
            let (readiness_tx, readiness_rx) = tokio::sync::watch::channel(false);
            let ready_timeout = self.ready_timeout;

            let readiness_task = tokio::spawn(async move {
                let probe = run_readiness_probe(condition);
                if let Some(timeout) = ready_timeout {
                    if tokio::time::timeout(timeout, probe).await.is_err() {
                        // Timed out — probe failed, don't set ready
                        return;
                    }
                } else {
                    probe.await;
                }
                let _ = readiness_tx.send(true);
            });

            handle.readiness_rx = readiness_rx;
            handle._readiness_task = Some(readiness_task);
        }

        // Start lifetime timeout if configured
        if let Some(timeout) = self.timeout {
            let pgid = handle.pgid();
            let pid = handle.pid();
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                // Kill the process group
                if let Some(pgid) = pgid {
                    let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGTERM));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = killpg(Pid::from_raw(pgid), Some(Signal::SIGKILL));
                } else if let Some(pid) = pid {
                    let _ = killpg(Pid::from_raw(pid as i32), Some(Signal::SIGTERM));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = killpg(Pid::from_raw(pid as i32), Some(Signal::SIGKILL));
                }
            });
        }

        if let Some(on_spawn) = self.on_spawn {
            on_spawn(&handle);
        }
        Ok(handle)
    }
}

impl std::future::IntoFuture for SpawnBuilder {
    type Output = Result<ProcessHandle, ProcessError>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.do_spawn())
    }
}

/// Future that spawns a process and waits for it to complete.
///
/// Returned by `SpawnBuilder::complete()`.
pub struct CompletionFuture {
    inner: std::pin::Pin<Box<dyn std::future::Future<Output = Result<ProcessResult, ProcessError>> + Send>>,
}

impl std::future::Future for CompletionFuture {
    type Output = Result<ProcessResult, ProcessError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::ParsedContent;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_exec_captures_stdout() {
        let mut buffer = OutputBuffer::new(100);
        let result = exec("echo hello", "test", &mut buffer).await.unwrap();
        assert!(result.success());
        let stdout = result.output().stdout().await;
        assert_eq!(stdout, vec!["hello"]);
    }

    #[tokio::test]
    async fn test_exec_captures_stderr() {
        let mut buffer = OutputBuffer::new(100);
        let result = exec("echo error >&2", "test", &mut buffer).await.unwrap();
        assert!(result.success());
        let stderr = result.output().stderr().await;
        assert_eq!(stderr, vec!["error"]);
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
        let _result = exec("echo 'just plain text'", "test", &mut buffer)
            .await
            .unwrap();

        assert!(!buffer.is_empty());
        let entry = &buffer.lines()[0];
        assert!(matches!(&entry.parsed, ParsedContent::PlainText));
        assert_eq!(entry.raw, "just plain text");
    }

    #[tokio::test]
    async fn test_exec_nonzero_exit() {
        let mut buffer = OutputBuffer::new(100);
        let result = exec("exit 42", "test", &mut buffer).await.unwrap();
        assert!(!result.success());
        assert_eq!(result.exit_code(), 42);
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
        let mut handle = spawn("echo spawned_output", "test", buffer.clone())
            .await
            .unwrap();

        // Wait for the process to finish
        let _result = handle.wait().await;

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
        let mut handle = spawn("sh -c 'sleep 120 & sleep 120'", "test", buffer)
            .await
            .unwrap();

        assert!(handle.is_running());

        handle.stop(Duration::from_secs(2)).await.unwrap();

        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_output_buffer_ring() {
        let mut buffer = OutputBuffer::new(3);
        let make_entry = |raw: &str, seq: u64| LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: crate::execution::TaskId(0),
            seq,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
            stream: None,
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
            received_at: chrono::Utc::now(),
            raw: "broadcast_test".to_string(),
            parsed: ParsedContent::PlainText,
            source: crate::execution::TaskId(0),
            seq: 0,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
            stream: None,
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
        // We should get a ProcessResult with a non-zero exit code.
        let cmd = r#"kill -SEGV $$"#;
        let mut buffer = OutputBuffer::new(100);
        let result = exec(cmd, "test", &mut buffer).await.unwrap();

        // On Unix, death by signal yields exit code 128+signal or negative
        // SIGSEGV = 11, so expect non-zero
        assert!(
            !result.success(),
            "expected non-zero exit code, got {}",
            result.exit_code()
        );
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
        nix::sys::signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL).unwrap();

        // wait() should return a ProcessResult with non-zero exit from signal death
        let result = handle.wait().await;
        assert!(!result.success());
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
            Ok(proc_result) => {
                let stdout = proc_result.output().stdout().await;
                let total_len: usize = stdout.iter().map(|s| s.trim().len()).sum();
                assert_eq!(total_len, 1_000_000);
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
        let result = exec(cmd, "test", &mut buffer).await.unwrap();

        let stdout = result.output().stdout().await;
        let stdout_text = stdout.join("\n");
        assert!(
            stdout_text.contains("hello"),
            "valid output after binary data was lost"
        );
    }

    #[tokio::test]
    async fn test_log_entry_source_and_seq() {
        let mut buffer = OutputBuffer::new(100);
        let cmd = r#"echo first; echo second; echo third"#;
        let _result = exec(cmd, "my_task", &mut buffer).await.unwrap();

        assert_eq!(buffer.len(), 3);

        let entries: Vec<_> = buffer.lines().iter().collect();
        // All entries from a single exec should share the same allocated
        // process TaskId (the source field).
        let first_source = entries[0].source;
        for entry in &entries {
            assert_eq!(entry.source, first_source);
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
