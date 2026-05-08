//! Human-readable task report renderer for the MCP layer.
//!
//! Pure formatter: takes a `GraphSnapshot`, a top-level task id, the tail
//! of `LogEntry`s for that task's descendant source set, and a tail size,
//! and produces the report string defined in `docs/mcp_design.md`
//! § "Task report" → "Format".
//!
//! The renderer does no I/O. The caller (Phase 6's MCP tool layer) is
//! responsible for fetching the snapshot and the relevant log entries —
//! either directly through an `EngineHandle`, or over the supervisor↔
//! engine wire — and handing them in.

use std::collections::HashSet;

use chrono::Local;

use crate::execution::TaskId;
use crate::execution::engine::GraphSnapshot;
use crate::execution::execution::TaskStatus;
use crate::log::{LogEntry, ParsedContent, Stream};

/// Default tail size when the caller doesn't specify `tail_n`.
pub const DEFAULT_TAIL_N: usize = 50;

/// Maximum tail size — guards against pathologically large reports.
/// Agents that want more should use `get_logs` with explicit pagination.
pub const MAX_TAIL_N: usize = 1000;

/// Render a human-readable task report for `top_id`.
///
/// `snapshot` is a single graph snapshot taken at any time (the engine's
/// `watch::Receiver<GraphSnapshot>::borrow()`, or the supervisor's
/// cached copy). `log_entries` must already be filtered to the
/// descendant source set (caller-fetched). `tail_n` controls the
/// "Last n lines:" fallback when no `Summary` is set.
///
/// If `top_id` doesn't appear in the snapshot, a single-line report is
/// produced (`Task t<id> ? - unknown`) so the caller has *something* to
/// hand back to the agent.
pub fn render(
    snapshot: &GraphSnapshot,
    top_id: TaskId,
    log_entries: &[LogEntry],
    tail_n: usize,
) -> String {
    let tail_n = tail_n.min(MAX_TAIL_N);

    let Some(node) = snapshot.tasks.get(&top_id) else {
        return format!("Task t{} ? - unknown\n", top_id.0);
    };

    let descendants: HashSet<TaskId> = snapshot.descendant_source_ids(top_id);
    let task_sources: HashSet<TaskId> = task_only_sources(snapshot, top_id);

    let mut out = String::new();

    // Line 1: Task <id> <name> - <status>
    out.push_str(&format!(
        "Task t{} {} - {}\n",
        top_id.0,
        node.name,
        format_status(&node.status)
    ));

    // Line 2: Started + Run time
    let (started_str, runtime_str) = format_started_and_runtime(node);
    out.push_str(&format!("Started: {}  Run time: {}\n", started_str, runtime_str));

    // Stdout / Stderr counts with format-detection suffix.
    let stdout_count = count_with_stream(log_entries, &descendants, Stream::Stdout);
    let stderr_count = count_with_stream(log_entries, &descendants, Stream::Stderr);
    let stdout_suffix = format_kind_suffix(log_entries, &descendants, Some(Stream::Stdout));
    let stderr_suffix = format_kind_suffix(log_entries, &descendants, Some(Stream::Stderr));
    out.push_str(&format!("Stdout: {} lines{}\n", stdout_count, stdout_suffix));
    out.push_str(&format!("Stderr: {} lines{}\n", stderr_count, stderr_suffix));

    // Events — entries from task-source ids (not process ids).
    let events_count = log_entries
        .iter()
        .filter(|e| task_sources.contains(&e.source))
        .count();
    out.push_str(&format!("Events: {} lines\n", events_count));

    // Summary OR "Last n lines:" fallback.
    if let Some(summary) = &node.summary {
        out.push_str("Summary:\n");
        out.push_str(summary);
        if !summary.ends_with('\n') {
            out.push('\n');
        }
    } else {
        out.push_str(&format!("Last {} lines:\n", tail_n));
        // Sort entries by seq (the engine-global cursor) and take the
        // last `tail_n`. Caller probably already did this, but we
        // re-sort defensively for stable output.
        let mut sorted: Vec<&LogEntry> = log_entries.iter().collect();
        sorted.sort_by_key(|e| e.seq);
        let start = sorted.len().saturating_sub(tail_n);
        let labels = snapshot.source_labels();
        for entry in &sorted[start..] {
            out.push_str(&crate::log::format::format_entry(entry, &labels));
            out.push('\n');
        }
    }

    out
}

/// Status formatter per design §"Task report".
fn format_status(status: &TaskStatus) -> String {
    match status {
        TaskStatus::Setup => "running (setup)".to_string(),
        TaskStatus::Ready => "running (ready)".to_string(),
        TaskStatus::Done => "completed (exit 0)".to_string(),
        TaskStatus::Failed(failure) => format!("failed: {}", failure.message),
        TaskStatus::Cancelled => "cancelled".to_string(),
        TaskStatus::Timeout => "timed out".to_string(),
    }
}

/// True when the task is still in a non-terminal status.
fn is_running(status: &TaskStatus) -> bool {
    matches!(status, TaskStatus::Setup | TaskStatus::Ready)
}

/// Build the `Started:` and `Run time:` strings.
///
/// - `Started:` is `YYYY-MM-DD HH:MM:SS` (or `(not started)`).
/// - `Run time:` uses `humantime::format_duration` rounded to seconds.
///   When the task is still running, append `(running)`.
fn format_started_and_runtime(node: &crate::execution::engine::TaskNode) -> (String, String) {
    let Some(started) = node.started_at else {
        return ("(not started)".to_string(), "-".to_string());
    };
    let started_str = started.format("%Y-%m-%d %H:%M:%S").to_string();

    let end = node
        .ended_at
        .unwrap_or_else(|| Local::now().with_timezone(&Local));
    let elapsed = (end - started).to_std().unwrap_or_default();
    // Round to whole seconds so the report stays readable.
    let secs = std::time::Duration::from_secs(elapsed.as_secs());
    let mut runtime_str = humantime::format_duration(secs).to_string();
    if is_running(&node.status) {
        runtime_str.push_str(" (running)");
    }
    (started_str, runtime_str)
}

/// Count log entries whose source is in `sources` and whose stream is
/// `stream`.
fn count_with_stream(entries: &[LogEntry], sources: &HashSet<TaskId>, stream: Stream) -> usize {
    entries
        .iter()
        .filter(|e| sources.contains(&e.source))
        .filter(|e| e.stream == Some(stream))
        .count()
}

/// Walk the snapshot subtree rooted at `top_id`, collecting only TaskNode
/// ids (no process ids). Used to identify "Events" — entries authored by
/// task code via `tracing` macros or `ctx.println`.
fn task_only_sources(snapshot: &GraphSnapshot, top_id: TaskId) -> HashSet<TaskId> {
    let mut out = HashSet::new();
    let mut stack = vec![top_id];
    while let Some(id) = stack.pop() {
        let Some(node) = snapshot.tasks.get(&id) else {
            continue;
        };
        out.insert(node.id);
        for &child in &node.children {
            stack.push(child);
        }
    }
    out
}

/// Build the `, JSON 91%` style suffix for `Stdout:` / `Stderr:` lines.
///
/// Walks `entries` filtered by `sources` and (optionally) `stream`,
/// counts each parsed-content kind, and reports the dominant non-PlainText
/// kind if it clears the 60% threshold. Returns an empty string otherwise.
fn format_kind_suffix(
    entries: &[LogEntry],
    sources: &HashSet<TaskId>,
    stream: Option<Stream>,
) -> String {
    let filtered: Vec<&LogEntry> = entries
        .iter()
        .filter(|e| sources.contains(&e.source))
        .filter(|e| match stream {
            Some(s) => e.stream == Some(s),
            None => true,
        })
        .collect();

    let total = filtered.len();
    if total == 0 {
        return String::new();
    }

    let mut json = 0usize;
    let mut logfmt = 0usize;
    for entry in &filtered {
        match &entry.parsed {
            ParsedContent::Json(_) => json += 1,
            ParsedContent::Logfmt(_) => logfmt += 1,
            ParsedContent::PlainText => {}
        }
    }

    // Pick the dominant non-PlainText kind.
    let (kind_name, kind_count) = if json >= logfmt {
        ("JSON", json)
    } else {
        ("Logfmt", logfmt)
    };

    // 60% threshold; integer math (count * 100 >= total * 60).
    if kind_count * 100 < total * 60 {
        return String::new();
    }
    let pct = (kind_count * 100) / total;
    format!(", {} {}%", kind_name, pct)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::{Duration, Local};

    use super::*;
    use crate::execution::TaskId;
    use crate::execution::engine::{GraphSnapshot, ProcessNodeInfo, TaskNode};
    use crate::execution::execution::{ProcessStatus, TaskFailure, TaskStatus};
    use crate::log::{LogEntry, ParsedContent, Stream};

    fn task_node(
        id: TaskId,
        name: &str,
        parent: Option<TaskId>,
        status: TaskStatus,
        summary: Option<String>,
    ) -> TaskNode {
        let started = Local::now() - Duration::seconds(10);
        let ended = Local::now();
        TaskNode {
            id,
            name: name.into(),
            parent,
            children: vec![],
            status,
            processes: vec![],
            started_at: Some(started),
            ended_at: Some(ended),
            summary,
        }
    }

    fn snapshot_with(top: TaskNode) -> GraphSnapshot {
        let mut tasks = HashMap::new();
        tasks.insert(top.id, top);
        GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks),
        }
    }

    fn make_entry(source: TaskId, seq: u64, raw: &str, stream: Option<Stream>) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source,
            seq,
            timestamp: None,
            level: None,
            message: Some(raw.to_string()),
            fields: HashMap::new(),
            stream,
        }
    }

    fn make_json_entry(source: TaskId, seq: u64, stream: Stream) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: r#"{"k":"v"}"#.into(),
            parsed: ParsedContent::Json(serde_json::json!({"k":"v"})),
            source,
            seq,
            timestamp: None,
            level: None,
            message: Some("k=v".into()),
            fields: HashMap::new(),
            stream: Some(stream),
        }
    }

    #[test]
    fn renders_completed_with_summary() {
        let id = TaskId(42);
        let mut node = task_node(
            id,
            "my-task",
            Some(TaskId::ROOT),
            TaskStatus::Done,
            Some("my summary text".into()),
        );
        node.parent = Some(TaskId::ROOT);
        let snap = snapshot_with(node);

        let report = render(&snap, id, &[], 50);

        assert!(
            report.starts_with("Task t42 my-task - completed (exit 0)\n"),
            "header wrong:\n{report}"
        );
        assert!(report.contains("Started: "), "no Started line:\n{report}");
        assert!(report.contains("Run time: "), "no Run time:\n{report}");
        assert!(report.contains("Summary:\nmy summary text"), "no summary:\n{report}");
        assert!(
            !report.contains("Last "),
            "Last n lines must be omitted when summary present:\n{report}"
        );
    }

    #[test]
    fn renders_last_n_lines_when_no_summary() {
        let id = TaskId(7);
        let node = task_node(id, "noisy", Some(TaskId::ROOT), TaskStatus::Done, None);
        let snap = snapshot_with(node);

        let entries = vec![
            make_entry(id, 1, "first line", None),
            make_entry(id, 2, "second line", None),
            make_entry(id, 3, "third line", None),
        ];

        let report = render(&snap, id, &entries, 50);
        assert!(report.contains("Last 50 lines:\n"), "no Last block:\n{report}");
        assert!(report.contains("first line"), "missing first:\n{report}");
        assert!(report.contains("second line"), "missing second:\n{report}");
        assert!(report.contains("third line"), "missing third:\n{report}");
        assert!(!report.contains("Summary:"), "summary must be absent:\n{report}");
    }

    #[test]
    fn renders_status_variants() {
        let id = TaskId(1);

        let cases: &[(TaskStatus, &str)] = &[
            (TaskStatus::Done, "completed (exit 0)"),
            (TaskStatus::Cancelled, "cancelled"),
            (TaskStatus::Timeout, "timed out"),
            (TaskStatus::Setup, "running (setup)"),
            (TaskStatus::Ready, "running (ready)"),
        ];
        for (status, expected) in cases {
            let node = task_node(id, "t", Some(TaskId::ROOT), status.clone(), None);
            let snap = snapshot_with(node);
            let report = render(&snap, id, &[], 50);
            assert!(
                report.contains(expected),
                "expected {expected:?} in:\n{report}"
            );
        }

        // Failed carries a message.
        let failure = TaskFailure {
            message: "boom".into(),
            exit_code: 1,
            output_json: "{}".into(),
        };
        let node = task_node(id, "t", Some(TaskId::ROOT), TaskStatus::Failed(failure), None);
        let snap = snapshot_with(node);
        let report = render(&snap, id, &[], 50);
        assert!(
            report.contains("failed: boom"),
            "expected 'failed: boom' in:\n{report}"
        );
    }

    #[test]
    fn running_status_appends_running_to_runtime() {
        let id = TaskId(9);
        let mut node = task_node(id, "t", Some(TaskId::ROOT), TaskStatus::Ready, None);
        // Simulate a still-running task: ended_at must be None.
        node.ended_at = None;
        let snap = snapshot_with(node);
        let report = render(&snap, id, &[], 50);
        assert!(
            report.contains("(running)"),
            "expected (running) suffix in Run time line:\n{report}"
        );
    }

    #[test]
    fn format_detection_dominant_json() {
        let id = TaskId(20);
        // Build a snapshot with one process child so stream entries from
        // a process source are counted.
        let process_id = TaskId(21);
        let mut top = task_node(id, "t", Some(TaskId::ROOT), TaskStatus::Done, None);
        top.processes.push(ProcessNodeInfo {
            id: process_id,
            task_name: "t".into(),
            command_label: "echo".into(),
            pid: None,
            pgid: None,
            status: ProcessStatus::Done,
            ready: false,
        });
        let snap = snapshot_with(top);

        let entries: Vec<LogEntry> = (0..100)
            .map(|i| make_json_entry(process_id, i + 1, Stream::Stdout))
            .collect();

        let report = render(&snap, id, &entries, 50);
        // Dominant kind clears 60%; the suffix uses integer-divided %.
        assert!(
            report.contains("Stdout: 100 lines, JSON 100%"),
            "expected dominant JSON suffix:\n{report}"
        );
    }

    #[test]
    fn format_detection_below_threshold_omitted() {
        let id = TaskId(30);
        let process_id = TaskId(31);
        let mut top = task_node(id, "t", Some(TaskId::ROOT), TaskStatus::Done, None);
        top.processes.push(ProcessNodeInfo {
            id: process_id,
            task_name: "t".into(),
            command_label: "echo".into(),
            pid: None,
            pgid: None,
            status: ProcessStatus::Done,
            ready: false,
        });
        let snap = snapshot_with(top);

        let mut entries = Vec::new();
        for i in 0..50 {
            entries.push(make_json_entry(process_id, i + 1, Stream::Stdout));
        }
        for i in 50..100 {
            entries.push(make_entry(process_id, i + 1, "plain", Some(Stream::Stdout)));
        }

        let report = render(&snap, id, &entries, 50);
        // 50% JSON < 60% threshold → no suffix.
        assert!(
            report.contains("Stdout: 100 lines\n"),
            "expected no suffix on stdout line:\n{report}"
        );
    }

    #[test]
    fn events_count_uses_task_sources_only() {
        let id = TaskId(60);
        let process_id = TaskId(61);
        let mut top = task_node(id, "t", Some(TaskId::ROOT), TaskStatus::Done, None);
        top.processes.push(ProcessNodeInfo {
            id: process_id,
            task_name: "t".into(),
            command_label: "echo".into(),
            pid: None,
            pgid: None,
            status: ProcessStatus::Done,
            ready: false,
        });
        let snap = snapshot_with(top);

        // 5 task-source entries (no stream — like ctx.println / tracing).
        let mut entries: Vec<LogEntry> = (0..5)
            .map(|i| make_entry(id, i + 1, "evt", None))
            .collect();
        // 3 process entries on stdout — should NOT count as events.
        for i in 0..3 {
            entries.push(make_entry(
                process_id,
                100 + i,
                "stdout",
                Some(Stream::Stdout),
            ));
        }

        let report = render(&snap, id, &entries, 50);
        assert!(
            report.contains("Events: 5 lines"),
            "expected Events: 5 lines:\n{report}"
        );
    }

    #[test]
    fn unknown_top_id_renders_unknown_line() {
        let snap = GraphSnapshot::default();
        let report = render(&snap, TaskId(999), &[], 50);
        assert!(
            report.starts_with("Task t999 ? - unknown"),
            "expected unknown line; got:\n{report}"
        );
    }

    #[test]
    fn tail_n_caps_at_max() {
        let id = TaskId(80);
        let node = task_node(id, "t", Some(TaskId::ROOT), TaskStatus::Done, None);
        let snap = snapshot_with(node);
        // tail_n way over MAX should still produce a capped header.
        let report = render(&snap, id, &[], 1_000_000);
        assert!(
            report.contains(&format!("Last {} lines:", MAX_TAIL_N)),
            "tail_n was not capped to MAX_TAIL_N:\n{report}"
        );
    }

}
