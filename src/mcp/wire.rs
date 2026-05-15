//! Wire protocol types for the supervisor↔engine RPC.
//!
//! Per the design doc § "Single source of truth: engine types ARE wire
//! types", this module deliberately does NOT define parallel copies of
//! engine types. The engine types (`TaskId`, `GraphSnapshot`, `TaskNode`,
//! `ProcessNodeInfo`, `TaskStatus`, `ProcessStatus`, `KillSignal`,
//! `SpawnOptions`, `LogEntry`, `TaskInfo`, `EngineError`) carry the
//! `Serialize` / `Deserialize` derives directly and are used in-place by
//! the variants below.
//!
//! Wire framing rule: every `WireMessage` serializes to a single line of
//! JSON (no embedded `\n`). The transport layer
//! ([`crate::mcp::transport`]) is the only place that calls
//! `serde_json::to_string` / `from_str`.

use serde::{Deserialize, Serialize};

use crate::execution::{EngineError, GraphSnapshot, KillSignal, RestartMode, SpawnOptions, TaskId};
use crate::log::LogEntry;
use crate::task::TaskInfo;

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// Correlates a `Request` with its `Response`.
///
/// Allocated by the supervisor (the requestor) and echoed back in the
/// matching `Response`. Process-local; not persisted across reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub u64);

/// Identifies a log subscription.
///
/// Allocated by the engine server in response to `SubscribeLogs` and
/// referenced by `UnsubscribeLogs` and by every `Event::Log` carrying
/// entries for that subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubscriptionId(pub u64);

// ---------------------------------------------------------------------------
// Top-level message
// ---------------------------------------------------------------------------

/// Top-level wire frame. Every line on the supervisor↔engine TCP socket
/// is exactly one `WireMessage` JSON-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireMessage {
    /// Supervisor → engine. The supervisor allocates the `id`.
    Request {
        id: CorrelationId,
        body: Request,
    },
    /// Engine → supervisor, paired with the matching `Request.id`.
    Response {
        id: CorrelationId,
        body: Result<Response, RpcError>,
    },
    /// Engine → supervisor, unsolicited. Streaming graph + log updates.
    Event(Event),
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Supervisor → engine RPC requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// List every registered task. Engine returns `Response::ListTasks`.
    ListTasks,
    /// Spawn a top-level task (child of the synthetic root). `args` are
    /// raw strings, parsed engine-side via the task's clap metadata.
    SpawnTask {
        name: String,
        args: Vec<String>,
        opts: SpawnOptions,
    },
    /// Cancel a task and its subtree.
    KillTask {
        task_id: TaskId,
        signal: KillSignal,
    },
    /// Signal a single spawned process. Does not affect task status.
    KillProcess {
        process_id: TaskId,
        signal: KillSignal,
    },
    /// Cancel every direct child of root.
    KillAll,
    /// Restart a top-level task. `Soft` delivers a cooperative signal
    /// if the task subscribed via `ctx.restart_handle()`; otherwise
    /// falls back to `Hard` (kill subtree + respawn).
    RestartTask {
        task_id: TaskId,
        mode: RestartMode,
    },
    /// Page through historical log entries for a task (and descendants).
    GetLogs {
        task_id: TaskId,
        since_seq: Option<u64>,
        until_seq: Option<u64>,
        limit: Option<u32>,
        filter: Option<String>,
    },
    /// Search log entries by pattern (substring or regex per scope rules).
    GrepLogs {
        task_id: TaskId,
        pattern: String,
        limit: Option<u32>,
        scope: GrepScope,
    },
    /// Open a streaming subscription. Subsequent `Event::Log` frames carry
    /// matching entries. Idempotent on supervisor reconnect — supervisor
    /// passes `from_seq` to resume.
    SubscribeLogs {
        task_id: TaskId,
        filter: Option<String>,
        from_seq: Option<u64>,
    },
    /// Close a previously opened subscription.
    UnsubscribeLogs {
        subscription_id: SubscriptionId,
    },
    /// Count log entries by stream class for a task and its descendants.
    /// Used by the report renderer (so counts don't depend on the
    /// `tail_n` slice the renderer happens to receive).
    CountLogs {
        task_id: TaskId,
    },
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Engine → supervisor RPC responses. Variant matches the corresponding
/// `Request` variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    ListTasks(Vec<TaskInfo>),
    SpawnTask {
        task_id: TaskId,
        /// Global `LogStore` seq at the moment the task was spawned. The
        /// supervisor passes this as `from_seq` on a follow-up
        /// `SubscribeLogs` to close the spawn-then-subscribe race.
        initial_seq: u64,
    },
    KillTask,
    KillProcess,
    KillAll,
    RestartTask {
        /// `TaskId` after restart. Equal to the request `task_id` for
        /// soft restarts that hit a subscriber; a fresh id for hard
        /// restarts (or soft restarts that fell back to hard).
        task_id: TaskId,
    },
    GetLogs {
        entries: Vec<LogEntry>,
        next_seq: u64,
        has_more: bool,
    },
    GrepLogs {
        matches: Vec<LogEntry>,
    },
    SubscribeLogs {
        subscription_id: SubscriptionId,
    },
    UnsubscribeLogs,
    CountLogs(LogCounts),
}

/// Per-stream log entry counts for a task subtree. Stdout and stderr are
/// totals across descendant *processes*; events are totals across
/// task-source ids (tracing macros, `ctx.println`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LogCounts {
    pub stdout: u64,
    pub stderr: u64,
    pub events: u64,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Unsolicited engine → supervisor frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Fresh task graph snapshot. Replaces any previously cached snapshot.
    Graph { snapshot: GraphSnapshot },
    /// One log entry matching an open subscription.
    Log {
        subscription_id: SubscriptionId,
        entry: LogEntry,
    },
}

// ---------------------------------------------------------------------------
// Errors and small enums
// ---------------------------------------------------------------------------

/// RPC-level error returned in the `Err` arm of `Response`.
///
/// `Engine` wraps an underlying [`EngineError`] (task not found, engine
/// shutting down, etc.). The remaining variants are wire-level: malformed
/// requests, filter parse failures, lookup misses for non-task ids, and
/// catch-all internal failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcError {
    Engine(EngineError),
    BadRequest(String),
    FilterParse(String),
    Internal(String),
    NotFound(String),
}

/// Scope for `GrepLogs`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GrepScope {
    /// Search `task_id` and every descendant (transitively).
    Descendants,
    /// Search only the given `task_id`'s own entries.
    SelfOnly,
}

// ---------------------------------------------------------------------------
// Tests — round-trip every WireMessage variant.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::execution::{ProcessNodeInfo, ProcessStatus, TaskNode, TaskStatus};
    use crate::log::{LogEntry, ParsedContent, Stream};

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

    /// Round-trip a `WireMessage` and assert no embedded newlines.
    fn round_trip(msg: WireMessage) {
        let s = serde_json::to_string(&msg).expect("serialize");
        assert!(
            !s.contains('\n'),
            "wire message must be a single line, found embedded newline: {s}"
        );
        let _back: WireMessage = serde_json::from_str(&s).expect("deserialize");
        // We do not assert structural equality (Eq is not implemented
        // across the deeply-nested engine types), but we DO insist the
        // re-encoded form matches the first encoding bit-for-bit.
        let s2 = serde_json::to_string(&_back).expect("re-serialize");
        assert_eq!(s, s2, "round-trip must be stable");
    }

    #[test]
    fn round_trip_request_list_tasks() {
        round_trip(WireMessage::Request {
            id: CorrelationId(1),
            body: Request::ListTasks,
        });
    }

    #[test]
    fn round_trip_request_spawn_task() {
        round_trip(WireMessage::Request {
            id: CorrelationId(2),
            body: Request::SpawnTask {
                name: "build".into(),
                args: vec!["--release".into()],
                opts: SpawnOptions {
                    timeout: Some(std::time::Duration::from_secs(30)),
                },
            },
        });
    }

    #[test]
    fn round_trip_request_kill_task() {
        round_trip(WireMessage::Request {
            id: CorrelationId(3),
            body: Request::KillTask {
                task_id: TaskId(9),
                signal: KillSignal::Term,
            },
        });
    }

    #[test]
    fn round_trip_request_kill_process() {
        round_trip(WireMessage::Request {
            id: CorrelationId(4),
            body: Request::KillProcess {
                process_id: TaskId(10),
                signal: KillSignal::Kill,
            },
        });
    }

    #[test]
    fn round_trip_request_kill_all() {
        round_trip(WireMessage::Request {
            id: CorrelationId(5),
            body: Request::KillAll,
        });
    }

    #[test]
    fn round_trip_request_get_logs() {
        round_trip(WireMessage::Request {
            id: CorrelationId(6),
            body: Request::GetLogs {
                task_id: TaskId(11),
                since_seq: Some(100),
                until_seq: None,
                limit: Some(50),
                filter: Some("level:error".into()),
            },
        });
    }

    #[test]
    fn round_trip_request_grep_logs() {
        round_trip(WireMessage::Request {
            id: CorrelationId(7),
            body: Request::GrepLogs {
                task_id: TaskId(12),
                pattern: "panic".into(),
                limit: Some(20),
                scope: GrepScope::Descendants,
            },
        });
        round_trip(WireMessage::Request {
            id: CorrelationId(8),
            body: Request::GrepLogs {
                task_id: TaskId(13),
                pattern: "panic".into(),
                limit: None,
                scope: GrepScope::SelfOnly,
            },
        });
    }

    #[test]
    fn round_trip_request_subscribe_unsubscribe() {
        round_trip(WireMessage::Request {
            id: CorrelationId(9),
            body: Request::SubscribeLogs {
                task_id: TaskId(14),
                filter: None,
                from_seq: Some(0),
            },
        });
        round_trip(WireMessage::Request {
            id: CorrelationId(10),
            body: Request::UnsubscribeLogs {
                subscription_id: SubscriptionId(99),
            },
        });
    }

    #[test]
    fn round_trip_response_list_tasks() {
        round_trip(WireMessage::Response {
            id: CorrelationId(1),
            body: Ok(Response::ListTasks(vec![sample_task_info()])),
        });
    }

    #[test]
    fn round_trip_response_spawn_task() {
        round_trip(WireMessage::Response {
            id: CorrelationId(2),
            body: Ok(Response::SpawnTask {
                task_id: TaskId(20),
                initial_seq: 1234,
            }),
        });
    }

    #[test]
    fn round_trip_response_kill_variants() {
        for r in [Response::KillTask, Response::KillProcess, Response::KillAll] {
            round_trip(WireMessage::Response {
                id: CorrelationId(3),
                body: Ok(r),
            });
        }
    }

    #[test]
    fn round_trip_response_get_logs() {
        round_trip(WireMessage::Response {
            id: CorrelationId(4),
            body: Ok(Response::GetLogs {
                entries: vec![sample_log_entry()],
                next_seq: 100,
                has_more: false,
            }),
        });
    }

    #[test]
    fn round_trip_response_grep_logs() {
        round_trip(WireMessage::Response {
            id: CorrelationId(5),
            body: Ok(Response::GrepLogs {
                matches: vec![sample_log_entry(), sample_log_entry()],
            }),
        });
    }

    #[test]
    fn round_trip_response_subscribe_unsubscribe() {
        round_trip(WireMessage::Response {
            id: CorrelationId(6),
            body: Ok(Response::SubscribeLogs {
                subscription_id: SubscriptionId(7),
            }),
        });
        round_trip(WireMessage::Response {
            id: CorrelationId(7),
            body: Ok(Response::UnsubscribeLogs),
        });
    }

    #[test]
    fn round_trip_response_error_variants() {
        let errs = [
            RpcError::Engine(crate::execution::EngineError::NotFound(TaskId(1))),
            RpcError::Engine(crate::execution::EngineError::ShuttingDown),
            RpcError::BadRequest("bad arg".into()),
            RpcError::FilterParse("unclosed paren".into()),
            RpcError::Internal("explosion".into()),
            RpcError::NotFound("subscription".into()),
        ];
        for err in errs {
            round_trip(WireMessage::Response {
                id: CorrelationId(8),
                body: Err(err),
            });
        }
    }

    #[test]
    fn round_trip_event_graph() {
        round_trip(WireMessage::Event(Event::Graph {
            snapshot: sample_graph_snapshot(),
        }));
    }

    #[test]
    fn round_trip_event_log() {
        round_trip(WireMessage::Event(Event::Log {
            subscription_id: SubscriptionId(42),
            entry: sample_log_entry(),
        }));
    }

    #[test]
    fn no_pretty_print_in_serialized_output() {
        // A defensive check: even nested structures must serialize compact.
        let msg = WireMessage::Event(Event::Graph {
            snapshot: sample_graph_snapshot(),
        });
        let s = serde_json::to_string(&msg).unwrap();
        assert!(!s.contains('\n'), "compact JSON must not contain newlines");
        assert!(
            !s.contains("  "),
            "compact JSON should not contain pretty-print double-space indentation"
        );
    }
}
