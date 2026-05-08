//! JSONL-framed transport for the supervisor↔engine wire protocol.
//!
//! This is the **single** place in the codebase that calls
//! `serde_json::to_string` on a [`WireMessage`]. Using compact serialization
//! here is a correctness invariant: `LinesCodec` frames on `\n`, so an
//! embedded newline (as produced by `serde_json::to_string_pretty`) would
//! corrupt the framing. The rule is enforced once here and never spread.
//!
//! # Usage
//!
//! Wrap any `AsyncRead + AsyncWrite + Unpin` stream (e.g. a `TcpStream` or a
//! `tokio::io::duplex` pipe) in a [`WireTransport`]:
//!
//! ```ignore
//! let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
//! let mut client = WireTransport::new(client_io);
//! let mut server = WireTransport::new(server_io);
//!
//! client.send(&msg).await?;
//! let received = server.recv().await?;
//! ```

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use super::wire::WireMessage;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during framed transport operations.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("framing error: {0}")]
    Codec(#[from] LinesCodecError),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The remote side closed the connection (EOF on `recv()`).
    #[error("connection closed")]
    Closed,
}

// ---------------------------------------------------------------------------
// WireTransport
// ---------------------------------------------------------------------------

/// Wraps any `AsyncRead + AsyncWrite + Unpin` stream and provides typed
/// [`WireMessage`] send/receive operations using newline-delimited JSON
/// framing.
///
/// Each message is serialized to a single compact JSON line. The underlying
/// [`LinesCodec`] guarantees one logical message per line, and the 16 MiB
/// line limit prevents a runaway log entry from allocating unboundedly.
pub struct WireTransport<S> {
    framed: Framed<S, LinesCodec>,
}

/// Maximum line length: 16 MiB.
///
/// A giant log entry (e.g. a minified JS source map) should still fit. If
/// we ever need to stream truly unbounded payloads, that's a sign the design
/// should use a chunked transfer or a side channel — not a larger limit here.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

impl<S: AsyncRead + AsyncWrite + Unpin> WireTransport<S> {
    /// Create a new [`WireTransport`] wrapping `stream`.
    pub fn new(stream: S) -> Self {
        let codec = LinesCodec::new_with_max_length(MAX_LINE_BYTES);
        Self {
            framed: Framed::new(stream, codec),
        }
    }

    /// Serialize `msg` to compact JSON and send it as a single line.
    ///
    /// This is the **only** call site for `serde_json::to_string` on a
    /// `WireMessage` in the entire codebase. Do not add others.
    pub async fn send(&mut self, msg: &WireMessage) -> Result<(), TransportError> {
        // Compact serialization — no embedded newlines.
        let line = serde_json::to_string(msg)?;
        // LinesCodec::encode appends '\n'.
        self.framed.send(line).await?;
        Ok(())
    }

    /// Receive the next line and deserialize it as a [`WireMessage`].
    ///
    /// Returns [`TransportError::Closed`] if the peer closed the connection.
    pub async fn recv(&mut self) -> Result<WireMessage, TransportError> {
        match self.framed.next().await {
            Some(Ok(line)) => {
                let msg = serde_json::from_str(&line)?;
                Ok(msg)
            }
            Some(Err(e)) => Err(e.into()),
            None => Err(TransportError::Closed),
        }
    }

    /// Split into independent send/receive halves so a reader task and
    /// a writer task can drive the transport concurrently without
    /// sharing a mutex on the underlying stream.
    pub fn into_split(self) -> (WireSink<S>, WireStream<S>) {
        let (sink, stream) = self.framed.split();
        (WireSink { inner: sink }, WireStream { inner: stream })
    }
}

/// Send half of a split [`WireTransport`].
pub struct WireSink<S> {
    inner: SplitSink<Framed<S, LinesCodec>, String>,
}

impl<S: AsyncWrite + Unpin> WireSink<S> {
    /// Serialize `msg` and write it as a single line.
    pub async fn send(&mut self, msg: &WireMessage) -> Result<(), TransportError> {
        let line = serde_json::to_string(msg)?;
        self.inner.send(line).await?;
        Ok(())
    }
}

/// Receive half of a split [`WireTransport`].
pub struct WireStream<S> {
    inner: SplitStream<Framed<S, LinesCodec>>,
}

impl<S: AsyncRead + Unpin> WireStream<S> {
    /// Read the next line and deserialize it as a [`WireMessage`].
    pub async fn recv(&mut self) -> Result<WireMessage, TransportError> {
        match self.inner.next().await {
            Some(Ok(line)) => Ok(serde_json::from_str(&line)?),
            Some(Err(e)) => Err(e.into()),
            None => Err(TransportError::Closed),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::io::duplex;

    use super::*;
    use crate::execution::{GraphSnapshot, KillSignal, ProcessNodeInfo, ProcessStatus, SpawnOptions, TaskId, TaskNode, TaskStatus};
    use crate::log::{LogEntry, ParsedContent, Stream};
    use crate::mcp::wire::{
        CorrelationId, Event, GrepScope, Request, Response, RpcError, SubscriptionId, WireMessage,
    };
    use crate::task::TaskInfo;

    // -----------------------------------------------------------------------
    // Fixture helpers (mirrored from wire.rs tests)
    // -----------------------------------------------------------------------

    fn sample_log_entry() -> LogEntry {
        LogEntry {
            raw: "hello world".into(),
            parsed: ParsedContent::PlainText,
            source: TaskId(7),
            seq: 42,
            received_at: chrono::Utc::now(),
            timestamp: Some("2024-01-01T00:00:00Z".into()),
            level: Some("info".into()),
            message: Some("hello world".into()),
            fields: HashMap::new(),
            stream: Some(Stream::Stdout),
        }
    }

    fn sample_graph_snapshot() -> GraphSnapshot {
        let mut tasks = HashMap::new();
        tasks.insert(
            TaskId(1),
            TaskNode {
                id: TaskId(1),
                name: "build".into(),
                parent: Some(TaskId::ROOT),
                status: TaskStatus::Ready,
                processes: vec![ProcessNodeInfo {
                    id: TaskId(2),
                    task_name: "build".into(),
                    command_label: "cargo build".into(),
                    pid: Some(12345),
                    pgid: Some(12345),
                    status: ProcessStatus::Running,
                    ready: true,
                }],
                started_at: Some(chrono::Local::now()),
                summary: Some("ok".into()),
                ..Default::default()
            },
        );
        GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks),
        }
    }

    fn sample_task_info() -> TaskInfo {
        TaskInfo {
            name: "build".into(),
            group: "".into(),
            description: Some("Build the project".into()),
            qualified_name: "build".into(),
            args_help: None,
        }
    }

    /// All message variants we want to exercise.
    fn all_messages() -> Vec<WireMessage> {
        vec![
            // --- Requests ---
            WireMessage::Request {
                id: CorrelationId(1),
                body: Request::ListTasks,
            },
            WireMessage::Request {
                id: CorrelationId(2),
                body: Request::SpawnTask {
                    name: "build".into(),
                    args: vec!["--release".into()],
                    opts: SpawnOptions {
                        timeout: Some(std::time::Duration::from_secs(30)),
                    },
                },
            },
            WireMessage::Request {
                id: CorrelationId(3),
                body: Request::KillTask {
                    task_id: TaskId(9),
                    signal: KillSignal::Term,
                },
            },
            WireMessage::Request {
                id: CorrelationId(4),
                body: Request::KillProcess {
                    process_id: TaskId(10),
                    signal: KillSignal::Kill,
                },
            },
            WireMessage::Request {
                id: CorrelationId(5),
                body: Request::KillAll,
            },
            WireMessage::Request {
                id: CorrelationId(6),
                body: Request::GetLogs {
                    task_id: TaskId(11),
                    since_seq: Some(100),
                    until_seq: None,
                    limit: Some(50),
                    filter: Some("level:error".into()),
                },
            },
            WireMessage::Request {
                id: CorrelationId(7),
                body: Request::GrepLogs {
                    task_id: TaskId(12),
                    pattern: "panic".into(),
                    limit: Some(20),
                    scope: GrepScope::Descendants,
                },
            },
            WireMessage::Request {
                id: CorrelationId(8),
                body: Request::SubscribeLogs {
                    task_id: TaskId(14),
                    filter: None,
                    from_seq: Some(0),
                },
            },
            WireMessage::Request {
                id: CorrelationId(9),
                body: Request::UnsubscribeLogs {
                    subscription_id: SubscriptionId(99),
                },
            },
            // --- Responses ---
            WireMessage::Response {
                id: CorrelationId(10),
                body: Ok(Response::ListTasks(vec![sample_task_info()])),
            },
            WireMessage::Response {
                id: CorrelationId(11),
                body: Ok(Response::SpawnTask {
                    task_id: TaskId(20),
                    initial_seq: 1234,
                }),
            },
            WireMessage::Response {
                id: CorrelationId(12),
                body: Ok(Response::KillTask),
            },
            WireMessage::Response {
                id: CorrelationId(13),
                body: Ok(Response::KillProcess),
            },
            WireMessage::Response {
                id: CorrelationId(14),
                body: Ok(Response::KillAll),
            },
            WireMessage::Response {
                id: CorrelationId(15),
                body: Ok(Response::GetLogs {
                    entries: vec![sample_log_entry()],
                    next_seq: 100,
                    has_more: false,
                }),
            },
            WireMessage::Response {
                id: CorrelationId(16),
                body: Ok(Response::GrepLogs {
                    matches: vec![sample_log_entry()],
                }),
            },
            WireMessage::Response {
                id: CorrelationId(17),
                body: Ok(Response::SubscribeLogs {
                    subscription_id: SubscriptionId(7),
                }),
            },
            WireMessage::Response {
                id: CorrelationId(18),
                body: Ok(Response::UnsubscribeLogs),
            },
            // Error responses
            WireMessage::Response {
                id: CorrelationId(19),
                body: Err(RpcError::BadRequest("bad arg".into())),
            },
            WireMessage::Response {
                id: CorrelationId(20),
                body: Err(RpcError::Engine(
                    crate::execution::EngineError::ShuttingDown,
                )),
            },
            // --- Events ---
            WireMessage::Event(Event::Graph {
                snapshot: sample_graph_snapshot(),
            }),
            WireMessage::Event(Event::Log {
                subscription_id: SubscriptionId(42),
                entry: sample_log_entry(),
            }),
        ]
    }

    // -----------------------------------------------------------------------
    // Test: round-trip every variant through duplex pipe
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn round_trip_all_variants() {
        let (client_io, server_io) = duplex(4 * 1024 * 1024);
        let mut sender = WireTransport::new(client_io);
        let mut receiver = WireTransport::new(server_io);

        let messages = all_messages();

        for msg in &messages {
            sender.send(msg).await.expect("send failed");
            let received = receiver.recv().await.expect("recv failed");

            // Structural equality check: re-serialize both sides and compare
            // the compact JSON strings (same approach as wire.rs tests).
            let expected_json = serde_json::to_string(msg).unwrap();
            let received_json = serde_json::to_string(&received).unwrap();
            assert_eq!(
                expected_json, received_json,
                "round-trip mismatch for message variant"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test: two back-to-back messages both arrive correctly (frame integrity)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn back_to_back_frame_integrity() {
        let (client_io, server_io) = duplex(1024 * 1024);
        let mut sender = WireTransport::new(client_io);
        let mut receiver = WireTransport::new(server_io);

        let msg_a = WireMessage::Request {
            id: CorrelationId(100),
            body: Request::ListTasks,
        };
        let msg_b = WireMessage::Request {
            id: CorrelationId(101),
            body: Request::KillAll,
        };

        // Send both messages before reading either.
        sender.send(&msg_a).await.expect("send a");
        sender.send(&msg_b).await.expect("send b");

        let recv_a = receiver.recv().await.expect("recv a");
        let recv_b = receiver.recv().await.expect("recv b");

        assert_eq!(
            serde_json::to_string(&msg_a).unwrap(),
            serde_json::to_string(&recv_a).unwrap(),
            "first message mismatch"
        );
        assert_eq!(
            serde_json::to_string(&msg_b).unwrap(),
            serde_json::to_string(&recv_b).unwrap(),
            "second message mismatch"
        );
    }

    // -----------------------------------------------------------------------
    // Test: closed connection returns TransportError::Closed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn recv_on_closed_returns_closed_error() {
        let (client_io, server_io) = duplex(1024);
        let mut receiver = WireTransport::new(server_io);

        // Drop the sender — this closes the write end, causing the receiver's
        // read to see EOF.
        drop(client_io);

        let err = receiver.recv().await.expect_err("expected Closed error");
        assert!(
            matches!(err, TransportError::Closed),
            "expected TransportError::Closed, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Test: partial line does not yield a message until '\n' arrives
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn partial_line_does_not_yield_until_newline() {
        use tokio::io::{duplex, AsyncWriteExt};

        let (mut write_half, read_half) = duplex(1024 * 1024);
        let mut receiver = WireTransport::new(read_half);

        // Serialize a valid message.
        let msg = WireMessage::Request {
            id: CorrelationId(42),
            body: Request::ListTasks,
        };
        let full_line = serde_json::to_string(&msg).unwrap();

        // Split the JSON bytes into two halves; send only the first half.
        let mid = full_line.len() / 2;
        let (first_half, second_half) = full_line.split_at(mid);

        write_half
            .write_all(first_half.as_bytes())
            .await
            .expect("write first half");
        write_half.flush().await.expect("flush");

        // recv() must not return yet — poll it with a short timeout.
        let recv_future = receiver.recv();
        let timeout_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), recv_future).await;

        assert!(
            timeout_result.is_err(),
            "recv() should not complete on a partial line"
        );

        // Now send the remainder + newline to complete the frame.
        write_half
            .write_all(second_half.as_bytes())
            .await
            .expect("write second half");
        write_half
            .write_all(b"\n")
            .await
            .expect("write newline");
        write_half.flush().await.expect("flush");

        // Now recv() must complete successfully.
        let received = receiver.recv().await.expect("recv after full line");
        let expected_json = serde_json::to_string(&msg).unwrap();
        let received_json = serde_json::to_string(&received).unwrap();
        assert_eq!(expected_json, received_json, "message mismatch after partial write");
    }
}
