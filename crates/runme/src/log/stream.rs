//! Re-streaming and export capabilities for the log engine.
//!
//! This module provides higher-level wrappers around the broadcast channel
//! pattern used by `OutputBuffer` and `LogStore`:
//!
//! - **Live tailing** -- subscribe to new entries as they arrive
//! - **Replay** -- re-stream historical entries from a buffer as an async stream
//! - **Filtered streaming** -- wrap any broadcast receiver with a filter
//! - **Export** -- dump entries to a `Write` impl in raw text or JSON lines format

use std::io::Write;

use tokio::sync::{broadcast, mpsc};

use super::LogEntry;

// ---------------------------------------------------------------------------
// Live tailing
// ---------------------------------------------------------------------------

/// Subscribe to a broadcast sender and receive new entries as they arrive.
///
/// This is a thin wrapper that makes the intent explicit. The returned receiver
/// can be used with `.recv().await` to get entries one at a time.
pub fn tail(tx: &broadcast::Sender<LogEntry>) -> broadcast::Receiver<LogEntry> {
    tx.subscribe()
}

/// Subscribe to a broadcast sender, filtering entries by source name.
///
/// Only entries whose `source` field matches the given name are yielded.
pub fn tail_source(
    tx: &broadcast::Sender<LogEntry>,
    source: String,
) -> FilteredStream<impl Fn(&LogEntry) -> bool> {
    let filter = move |entry: &LogEntry| entry.source == source;
    FilteredStream {
        rx: tx.subscribe(),
        filter,
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Replay historical entries as an async stream via an mpsc channel.
///
/// Spawns a background task that sends each entry from the provided `Vec`
/// through the channel. The caller receives entries via the returned
/// `mpsc::Receiver`.
///
/// This is useful for replaying captured output through the same pipeline
/// that handles live data.
pub fn replay(entries: Vec<LogEntry>) -> mpsc::Receiver<LogEntry> {
    let (tx, rx) = mpsc::channel(entries.len().max(1));
    tokio::spawn(async move {
        for entry in entries {
            if tx.send(entry).await.is_err() {
                break; // receiver dropped
            }
        }
    });
    rx
}

/// Replay historical entries synchronously, returning an iterator.
///
/// For cases where async is not needed (e.g., piping directly to export).
pub fn replay_iter(entries: Vec<LogEntry>) -> impl Iterator<Item = LogEntry> {
    entries.into_iter()
}

// ---------------------------------------------------------------------------
// Filtered streaming
// ---------------------------------------------------------------------------

/// A filtered stream that wraps a broadcast receiver and only yields entries
/// matching a predicate.
///
/// This is a generic version of `store::FilteredSubscription` -- it works with
/// any `broadcast::Receiver<LogEntry>`, not just one obtained from a `LogStore`.
pub struct FilteredStream<F> {
    rx: broadcast::Receiver<LogEntry>,
    filter: F,
}

impl<F> FilteredStream<F>
where
    F: Fn(&LogEntry) -> bool,
{
    /// Create a new filtered stream from a broadcast receiver and a filter.
    pub fn new(rx: broadcast::Receiver<LogEntry>, filter: F) -> Self {
        Self { rx, filter }
    }

    /// Receive the next entry that matches the filter.
    ///
    /// Skips non-matching entries. Returns errors from the underlying broadcast
    /// channel (lagged or closed).
    pub async fn recv(&mut self) -> Result<LogEntry, broadcast::error::RecvError> {
        loop {
            let entry = self.rx.recv().await?;
            if (self.filter)(&entry) {
                return Ok(entry);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Export format for log entries.
pub enum ExportFormat {
    /// Raw text: just the `raw` field of each entry, one per line.
    Raw,
    /// JSON lines: each entry serialized as a single JSON object per line.
    JsonLines,
}

/// Export log entries as raw text (just the `raw` field, one per line).
///
/// Each entry's `raw` field is written followed by a newline.
pub fn export_raw(entries: &[LogEntry], writer: &mut impl Write) -> std::io::Result<()> {
    for entry in entries {
        writer.write_all(entry.raw.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

/// Export log entries as JSON lines (one JSON object per line).
///
/// Each entry is serialized as a complete JSON object on a single line.
/// Requires `LogEntry` to implement `serde::Serialize`.
pub fn export_jsonl(entries: &[LogEntry], writer: &mut impl Write) -> std::io::Result<()> {
    for entry in entries {
        serde_json::to_writer(&mut *writer, entry).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

/// Export log entries in the specified format.
pub fn export(
    entries: &[LogEntry],
    writer: &mut impl Write,
    format: ExportFormat,
) -> std::io::Result<()> {
    match format {
        ExportFormat::Raw => export_raw(entries, writer),
        ExportFormat::JsonLines => export_jsonl(entries, writer),
    }
}

/// Async export: write log entries to an `AsyncWrite` implementor as raw text.
pub async fn export_raw_async(
    entries: &[LogEntry],
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    for entry in entries {
        writer.write_all(entry.raw.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await
}

/// Async export: write log entries to an `AsyncWrite` implementor as JSON lines.
pub async fn export_jsonl_async(
    entries: &[LogEntry],
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    for entry in entries {
        let json = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
        writer.write_all(&json).await?;
        writer.write_all(b"\n").await?;
    }
    writer.flush().await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::ParsedContent;
    use std::collections::HashMap;

    /// Helper to create a LogEntry for testing.
    fn make_entry(source: &str, seq: u64, raw: &str) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
        }
    }

    /// Helper to create a LogEntry with level and message.
    fn make_entry_full(
        source: &str,
        seq: u64,
        raw: &str,
        level: Option<&str>,
        message: Option<&str>,
    ) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq,
            timestamp: None,
            level: level.map(|s| s.to_string()),
            message: message.map(|s| s.to_string()),
            fields: HashMap::new(),
        }
    }

    /// Helper to create a LogEntry with JSON parsed content.
    fn make_json_entry(source: &str, seq: u64, raw: &str, json: serde_json::Value) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::Json(json.clone()),
            source: source.to_string(),
            seq,
            timestamp: None,
            level: json.get("level").and_then(|v| v.as_str()).map(String::from),
            message: json.get("msg").and_then(|v| v.as_str()).map(String::from),
            fields: HashMap::new(),
        }
    }

    // ---------------------------------------------------------------
    // Live tailing tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_tail_receives_entries() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let mut rx = tail(&tx);

        let _ = tx.send(make_entry("app", 0, "hello world"));
        let received = rx.recv().await.unwrap();
        assert_eq!(received.raw, "hello world");
        assert_eq!(received.source, "app");
    }

    #[tokio::test]
    async fn test_tail_multiple_entries() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let mut rx = tail(&tx);

        let _ = tx.send(make_entry("a", 0, "first"));
        let _ = tx.send(make_entry("b", 1, "second"));
        let _ = tx.send(make_entry("a", 2, "third"));

        assert_eq!(rx.recv().await.unwrap().raw, "first");
        assert_eq!(rx.recv().await.unwrap().raw, "second");
        assert_eq!(rx.recv().await.unwrap().raw, "third");
    }

    #[tokio::test]
    async fn test_tail_source_filters_by_source() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let mut filtered = tail_source(&tx, "important".to_string());

        let _ = tx.send(make_entry("noise", 0, "ignore me"));
        let _ = tx.send(make_entry("important", 1, "pay attention"));
        let _ = tx.send(make_entry("noise", 2, "also ignore"));

        let received = filtered.recv().await.unwrap();
        assert_eq!(received.raw, "pay attention");
        assert_eq!(received.source, "important");
    }

    #[tokio::test]
    async fn test_tail_multiple_subscribers() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let mut rx1 = tail(&tx);
        let mut rx2 = tail(&tx);

        let _ = tx.send(make_entry("app", 0, "broadcast"));

        assert_eq!(rx1.recv().await.unwrap().raw, "broadcast");
        assert_eq!(rx2.recv().await.unwrap().raw, "broadcast");
    }

    // ---------------------------------------------------------------
    // Replay tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_replay_all_entries() {
        let entries = vec![
            make_entry("app", 0, "line 0"),
            make_entry("app", 1, "line 1"),
            make_entry("app", 2, "line 2"),
        ];

        let mut rx = replay(entries);
        assert_eq!(rx.recv().await.unwrap().raw, "line 0");
        assert_eq!(rx.recv().await.unwrap().raw, "line 1");
        assert_eq!(rx.recv().await.unwrap().raw, "line 2");
        // Channel closes after all entries sent
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_replay_empty() {
        let entries: Vec<LogEntry> = vec![];
        let mut rx = replay(entries);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_replay_preserves_order() {
        let entries: Vec<LogEntry> = (0..100)
            .map(|i| make_entry("src", i, &format!("line {i}")))
            .collect();

        let mut rx = replay(entries);
        for i in 0..100u64 {
            let entry = rx.recv().await.unwrap();
            assert_eq!(entry.seq, i);
            assert_eq!(entry.raw, format!("line {i}"));
        }
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn test_replay_iter() {
        let entries = vec![
            make_entry("app", 0, "line 0"),
            make_entry("app", 1, "line 1"),
        ];

        let collected: Vec<_> = replay_iter(entries).collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].raw, "line 0");
        assert_eq!(collected[1].raw, "line 1");
    }

    #[test]
    fn test_replay_iter_empty() {
        let entries: Vec<LogEntry> = vec![];
        let collected: Vec<_> = replay_iter(entries).collect();
        assert!(collected.is_empty());
    }

    // ---------------------------------------------------------------
    // Filtered streaming tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_filtered_stream_by_level() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let rx = tx.subscribe();
        let mut filtered =
            FilteredStream::new(rx, |e: &LogEntry| e.level.as_deref() == Some("error"));

        let _ = tx.send(make_entry_full("app", 0, "info msg", Some("info"), None));
        let _ = tx.send(make_entry_full("app", 1, "error msg", Some("error"), None));
        let _ = tx.send(make_entry_full("app", 2, "debug msg", Some("debug"), None));

        let received = filtered.recv().await.unwrap();
        assert_eq!(received.raw, "error msg");
        assert_eq!(received.level.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn test_filtered_stream_by_content() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let rx = tx.subscribe();
        let mut filtered = FilteredStream::new(rx, |e: &LogEntry| e.raw.contains("ERROR"));

        let _ = tx.send(make_entry("app", 0, "INFO: starting"));
        let _ = tx.send(make_entry("app", 1, "ERROR: disk full"));
        let _ = tx.send(make_entry("app", 2, "INFO: done"));

        let received = filtered.recv().await.unwrap();
        assert_eq!(received.raw, "ERROR: disk full");
    }

    #[tokio::test]
    async fn test_filtered_stream_all_filtered_out() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let rx = tx.subscribe();
        let mut filtered = FilteredStream::new(rx, |_: &LogEntry| false);

        let _ = tx.send(make_entry("app", 0, "entry"));
        // Drop sender to close the channel
        drop(tx);

        // recv should return an error (closed) since nothing matches
        let result = filtered.recv().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_filtered_stream_custom_closure() {
        let (tx, _) = broadcast::channel::<LogEntry>(16);
        let rx = tx.subscribe();
        // Filter: only even sequence numbers
        let mut filtered = FilteredStream::new(rx, |e: &LogEntry| e.seq % 2 == 0);

        let _ = tx.send(make_entry("app", 0, "even"));
        let _ = tx.send(make_entry("app", 1, "odd"));
        let _ = tx.send(make_entry("app", 2, "even again"));

        assert_eq!(filtered.recv().await.unwrap().raw, "even");
        assert_eq!(filtered.recv().await.unwrap().raw, "even again");
    }

    // ---------------------------------------------------------------
    // Export: raw text tests
    // ---------------------------------------------------------------

    #[test]
    fn test_export_raw_basic() {
        let entries = vec![
            make_entry("app", 0, "line one"),
            make_entry("app", 1, "line two"),
            make_entry("app", 2, "line three"),
        ];

        let mut output = Vec::new();
        export_raw(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text, "line one\nline two\nline three\n");
    }

    #[test]
    fn test_export_raw_empty() {
        let entries: Vec<LogEntry> = vec![];
        let mut output = Vec::new();
        export_raw(&entries, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn test_export_raw_preserves_content() {
        let entries = vec![make_entry(
            "app",
            0,
            r#"{"level":"error","msg":"disk full"}"#,
        )];

        let mut output = Vec::new();
        export_raw(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text, "{\"level\":\"error\",\"msg\":\"disk full\"}\n");
    }

    #[test]
    fn test_export_raw_multiple_sources() {
        let entries = vec![
            make_entry("src_a", 0, "from A"),
            make_entry("src_b", 1, "from B"),
            make_entry("src_a", 2, "from A again"),
        ];

        let mut output = Vec::new();
        export_raw(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["from A", "from B", "from A again"]);
    }

    // ---------------------------------------------------------------
    // Export: JSON lines tests
    // ---------------------------------------------------------------

    #[test]
    fn test_export_jsonl_basic() {
        let entries = vec![make_entry("app", 0, "hello"), make_entry("app", 1, "world")];

        let mut output = Vec::new();
        export_jsonl(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line should be valid JSON
        let parsed0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed0["raw"], "hello");
        assert_eq!(parsed0["source"], "app");
        assert_eq!(parsed0["seq"], 0);

        let parsed1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed1["raw"], "world");
        assert_eq!(parsed1["seq"], 1);
    }

    #[test]
    fn test_export_jsonl_empty() {
        let entries: Vec<LogEntry> = vec![];
        let mut output = Vec::new();
        export_jsonl(&entries, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn test_export_jsonl_with_fields() {
        let entries = vec![make_entry_full(
            "app",
            0,
            r#"{"level":"error","msg":"failed"}"#,
            Some("error"),
            Some("failed"),
        )];

        let mut output = Vec::new();
        export_jsonl(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["level"], "error");
        assert_eq!(parsed["message"], "failed");
        assert_eq!(parsed["source"], "app");
    }

    #[test]
    fn test_export_jsonl_with_json_parsed_content() {
        let json = serde_json::json!({"level": "info", "msg": "startup"});
        let entries = vec![make_json_entry(
            "app",
            0,
            r#"{"level":"info","msg":"startup"}"#,
            json,
        )];

        let mut output = Vec::new();
        export_jsonl(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        // The parsed field should contain the JSON representation of ParsedContent::Json
        assert!(parsed.get("parsed").is_some());
        assert_eq!(parsed["source"], "app");
    }

    #[test]
    fn test_export_jsonl_roundtrip() {
        let entries = vec![
            make_entry_full("app", 0, "line 0", Some("info"), Some("hello")),
            make_entry("app", 1, "line 1"),
        ];

        let mut output = Vec::new();
        export_jsonl(&entries, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        for line in text.lines() {
            // Each line should be independently parseable JSON
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.is_object());
            assert!(parsed.get("raw").is_some());
            assert!(parsed.get("source").is_some());
            assert!(parsed.get("seq").is_some());
        }
    }

    // ---------------------------------------------------------------
    // Export: format dispatch tests
    // ---------------------------------------------------------------

    #[test]
    fn test_export_format_raw() {
        let entries = vec![make_entry("app", 0, "raw line")];
        let mut output = Vec::new();
        export(&entries, &mut output, ExportFormat::Raw).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "raw line\n");
    }

    #[test]
    fn test_export_format_jsonl() {
        let entries = vec![make_entry("app", 0, "json line")];
        let mut output = Vec::new();
        export(&entries, &mut output, ExportFormat::JsonLines).unwrap();
        let text = String::from_utf8(output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["raw"], "json line");
    }

    // ---------------------------------------------------------------
    // Async export tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_export_raw_async() {
        let entries = vec![
            make_entry("app", 0, "async line 1"),
            make_entry("app", 1, "async line 2"),
        ];

        let mut output = Vec::new();
        export_raw_async(&entries, &mut output).await.unwrap();

        let text = String::from_utf8(output).unwrap();
        assert_eq!(text, "async line 1\nasync line 2\n");
    }

    #[tokio::test]
    async fn test_export_jsonl_async() {
        let entries = vec![make_entry("app", 0, "async json")];

        let mut output = Vec::new();
        export_jsonl_async(&entries, &mut output).await.unwrap();

        let text = String::from_utf8(output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(parsed["raw"], "async json");
        assert_eq!(parsed["source"], "app");
    }

    #[tokio::test]
    async fn test_export_raw_async_empty() {
        let entries: Vec<LogEntry> = vec![];
        let mut output = Vec::new();
        export_raw_async(&entries, &mut output).await.unwrap();
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn test_export_jsonl_async_empty() {
        let entries: Vec<LogEntry> = vec![];
        let mut output = Vec::new();
        export_jsonl_async(&entries, &mut output).await.unwrap();
        assert!(output.is_empty());
    }

    // ---------------------------------------------------------------
    // Integration with OutputBuffer subscription
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_integration_output_buffer_tail() {
        use crate::log::buffer::OutputBuffer;

        let mut buffer = OutputBuffer::new(100);
        // Subscribe via tail()
        let mut rx = tail(&buffer.subscribe_sender());

        buffer.push(make_entry("task", 0, "from buffer"));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.raw, "from buffer");
    }

    #[tokio::test]
    async fn test_integration_output_buffer_filtered() {
        use crate::log::buffer::OutputBuffer;

        let mut buffer = OutputBuffer::new(100);
        let rx = buffer.subscribe_sender().subscribe();
        let mut filtered = FilteredStream::new(rx, |e: &LogEntry| e.raw.contains("important"));

        buffer.push(make_entry("task", 0, "noise"));
        buffer.push(make_entry("task", 1, "important message"));
        buffer.push(make_entry("task", 2, "more noise"));

        let received = filtered.recv().await.unwrap();
        assert_eq!(received.raw, "important message");
    }

    #[tokio::test]
    async fn test_integration_replay_from_buffer() {
        use crate::log::buffer::OutputBuffer;

        let mut buffer = OutputBuffer::new(100);
        buffer.push(make_entry("task", 0, "line 0"));
        buffer.push(make_entry("task", 1, "line 1"));
        buffer.push(make_entry("task", 2, "line 2"));

        // Replay the buffer contents
        let entries: Vec<LogEntry> = buffer.lines().iter().cloned().collect();
        let mut rx = replay(entries);

        assert_eq!(rx.recv().await.unwrap().raw, "line 0");
        assert_eq!(rx.recv().await.unwrap().raw, "line 1");
        assert_eq!(rx.recv().await.unwrap().raw, "line 2");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_integration_export_buffer_contents() {
        use crate::log::buffer::OutputBuffer;

        let mut buffer = OutputBuffer::new(100);
        buffer.push(make_entry("task", 0, "line A"));
        buffer.push(make_entry("task", 1, "line B"));

        // Export as raw text
        let entries: Vec<LogEntry> = buffer.lines().iter().cloned().collect();
        let mut raw_output = Vec::new();
        export_raw(&entries, &mut raw_output).unwrap();
        assert_eq!(String::from_utf8(raw_output).unwrap(), "line A\nline B\n");

        // Export as JSONL
        let mut jsonl_output = Vec::new();
        export_jsonl(&entries, &mut jsonl_output).unwrap();
        let jsonl_text = String::from_utf8(jsonl_output).unwrap();
        let lines: Vec<&str> = jsonl_text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["raw"], "line A");
    }

    #[tokio::test]
    async fn test_integration_log_store_tail() {
        use crate::log::store::LogStore;

        let mut store = LogStore::new();
        let mut rx = tail(&store.sender());

        store.push(make_entry("task_a", 0, "from store"));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.raw, "from store");
    }

    #[tokio::test]
    async fn test_integration_log_store_replay_and_export() {
        use crate::log::store::LogStore;

        let mut store = LogStore::new();
        store.push(make_entry("a", 0, "first"));
        store.push(make_entry("b", 1, "second"));
        store.push(make_entry("a", 2, "third"));

        // Compose and export
        let composed = store.compose_owned();
        let mut output = Vec::new();
        export_raw(&composed, &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["first", "second", "third"]);
    }
}
