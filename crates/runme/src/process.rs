use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;

/// Captured output from a process execution.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
}

/// A line of captured output, possibly structured JSON.
#[derive(Debug, Clone)]
pub enum LogLine {
    Raw(String),
    Structured(serde_json::Value),
}

impl LogLine {
    /// Parse a line, detecting JSON.
    pub fn from_line(line: &str) -> Self {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(val) if val.is_object() || val.is_array() => LogLine::Structured(val),
            _ => LogLine::Raw(line.to_string()),
        }
    }

    /// Get the raw string representation.
    pub fn as_str(&self) -> String {
        match self {
            LogLine::Raw(s) => s.clone(),
            LogLine::Structured(v) => v.to_string(),
        }
    }
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
/// Stores lines with bounded capacity. When full, oldest lines are dropped.
/// Also broadcasts new lines to subscribers.
pub struct OutputBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    tx: broadcast::Sender<LogLine>,
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

    /// Push a line into the buffer. Drops the oldest if at capacity.
    pub fn push(&mut self, line: LogLine) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        // Broadcast to subscribers (ignore errors — no receivers is OK)
        let _ = self.tx.send(line.clone());
        self.lines.push_back(line);
    }

    /// Get all buffered lines.
    pub fn lines(&self) -> &VecDeque<LogLine> {
        &self.lines
    }

    /// Get a broadcast receiver for new lines.
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    /// Number of lines currently in the buffer.
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
}

/// Handle to a running child process.
///
/// The child is spawned in its own process group, allowing group-wide
/// signal delivery for clean shutdown.
pub struct ProcessHandle {
    child: tokio::process::Child,
    task_name: String,
    pgid: Option<i32>,
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
fn build_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c");
    cmd.arg(command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Spawn in a new process group
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            Ok(())
        });
    }

    cmd
}

/// Execute a command synchronously (wait for completion), capturing all output.
///
/// Lines are parsed for JSON structure and pushed into the provided OutputBuffer.
pub async fn exec(
    command: &str,
    task_name: &str,
    buffer: &mut OutputBuffer,
) -> Result<ExecOutput, ProcessError> {
    let mut child = build_command(command).spawn().map_err(ProcessError::Spawn)?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut stdout_text = String::new();
    let mut stderr_text = String::new();

    // Read stdout and stderr concurrently
    loop {
        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        stdout_text.push_str(&l);
                        stdout_text.push('\n');
                        buffer.push(LogLine::from_line(&l));
                    }
                    Ok(None) => {
                        // stdout closed, drain stderr
                        while let Ok(Some(l)) = stderr_reader.next_line().await {
                            stderr_text.push_str(&l);
                            stderr_text.push('\n');
                            buffer.push(LogLine::from_line(&l));
                        }
                        break;
                    }
                    Err(_) => break,
                }
            }
            line = stderr_reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        stderr_text.push_str(&l);
                        stderr_text.push('\n');
                        buffer.push(LogLine::from_line(&l));
                    }
                    Ok(None) => {
                        // stderr closed, drain stdout
                        while let Ok(Some(l)) = stdout_reader.next_line().await {
                            stdout_text.push_str(&l);
                            stdout_text.push('\n');
                            buffer.push(LogLine::from_line(&l));
                        }
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    }

    let status = child.wait().await.map_err(ProcessError::Wait)?;

    let _ = task_name; // used for future logging context

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
pub async fn spawn(
    command: &str,
    task_name: &str,
    buffer: std::sync::Arc<tokio::sync::Mutex<OutputBuffer>>,
) -> Result<ProcessHandle, ProcessError> {
    let mut child = build_command(command).spawn().map_err(ProcessError::Spawn)?;

    let pgid = child.id().map(|id| id as i32);

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Background task: read stdout lines into buffer
    let buf_clone = buffer.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let log_line = LogLine::from_line(&line);
            buf_clone.lock().await.push(log_line);
        }
    });

    // Background task: read stderr lines into buffer
    let buf_clone = buffer.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let log_line = LogLine::from_line(&line);
            buf_clone.lock().await.push(log_line);
        }
    });

    Ok(ProcessHandle {
        child,
        task_name: task_name.to_string(),
        pgid,
        _stdout_task: Some(stdout_task),
        _stderr_task: Some(stderr_task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        match &buffer.lines()[0] {
            LogLine::Structured(val) => {
                assert_eq!(val["level"], "info");
                assert_eq!(val["message"], "hello");
            }
            LogLine::Raw(s) => panic!("Expected Structured, got Raw: {}", s),
        }
    }

    #[tokio::test]
    async fn test_exec_raw_lines_not_json() {
        let mut buffer = OutputBuffer::new(100);
        let _result = exec("echo 'just plain text'", "test", &mut buffer).await.unwrap();

        assert!(!buffer.is_empty());
        match &buffer.lines()[0] {
            LogLine::Raw(s) => assert_eq!(s, "just plain text"),
            LogLine::Structured(_) => panic!("Expected Raw, got Structured"),
        }
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
        match &buf.lines()[0] {
            LogLine::Raw(s) => assert_eq!(s, "spawned_output"),
            LogLine::Structured(_) => panic!("Expected Raw"),
        }
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
        buffer.push(LogLine::Raw("line1".to_string()));
        buffer.push(LogLine::Raw("line2".to_string()));
        buffer.push(LogLine::Raw("line3".to_string()));
        assert_eq!(buffer.len(), 3);

        // Push one more, oldest should be dropped
        buffer.push(LogLine::Raw("line4".to_string()));
        assert_eq!(buffer.len(), 3);

        let lines: Vec<String> = buffer.lines().iter().map(|l| l.as_str()).collect();
        assert_eq!(lines, vec!["line2", "line3", "line4"]);
    }

    #[tokio::test]
    async fn test_output_buffer_subscribe() {
        let mut buffer = OutputBuffer::new(100);
        let mut rx = buffer.subscribe();

        buffer.push(LogLine::Raw("broadcast_test".to_string()));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.as_str(), "broadcast_test");
    }

    #[tokio::test]
    async fn test_logline_json_detection() {
        // Objects are structured
        let obj = LogLine::from_line(r#"{"key": "value"}"#);
        assert!(matches!(obj, LogLine::Structured(_)));

        // Arrays are structured
        let arr = LogLine::from_line(r#"[1, 2, 3]"#);
        assert!(matches!(arr, LogLine::Structured(_)));

        // Plain strings are raw even if valid JSON
        let plain = LogLine::from_line(r#""just a string""#);
        assert!(matches!(plain, LogLine::Raw(_)));

        // Numbers are raw
        let num = LogLine::from_line("42");
        assert!(matches!(num, LogLine::Raw(_)));

        // Non-JSON is raw
        let text = LogLine::from_line("hello world");
        assert!(matches!(text, LogLine::Raw(_)));
    }
}
