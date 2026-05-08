pub mod buffer;
pub mod extract;
pub mod field_stats;
pub mod filter;
pub mod format;
pub mod parse;
pub mod search;
pub mod store;
pub mod stream;

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::execution::TaskId;

/// Engine-global monotonic sequence generator for `LogEntry.seq`.
///
/// Cloning shares the underlying counter (it's an `Arc<AtomicU64>`). Engine
/// constructs one at startup; every `OutputBuffer` / `LogStore` /
/// `LogEntryLayer` shares clones so all entries — across all sources — get
/// strictly monotonically increasing seqs.
#[derive(Clone, Debug)]
pub struct SeqGen(Arc<AtomicU64>);

impl SeqGen {
    /// Create a fresh generator starting at 0 (so the first allocated seq is 1).
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// Allocate the next seq. Strictly monotonic across all clones; never returns 0.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Read the most recently allocated seq without advancing the counter.
    ///
    /// Returns `0` before any allocation. Used by frontends (e.g. the MCP
    /// engine server) that need a "seq just before this point" snapshot to
    /// hand to a follow-up subscription's `from_seq` so no entries between
    /// the snapshot and the subscription are missed.
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for SeqGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Which output stream a log entry originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Result of attempting to parse a record from the input buffer.
pub enum ParseResult {
    /// Successfully parsed a complete record. `usize` is bytes consumed from input.
    Record(RawRecord, usize),
    /// This parser doesn't handle this input -- try the next one.
    Rejection,
    /// Input could be a partial record in this format -- need more data.
    Incomplete,
}

/// A raw record as produced by a parser, before field extraction.
pub struct RawRecord {
    pub raw: String,
    pub parsed: ParsedContent,
}

/// How the record was parsed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ParsedContent {
    Json(serde_json::Value),
    Logfmt(Vec<(String, String)>),
    PlainText,
}

/// Fields extracted from a parsed record.
pub struct ExtractedFields {
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,
    pub fields: HashMap<String, serde_json::Value>,
}

/// The universal log record. Everything downstream works with this type.
///
/// Must implement Clone (required by `tokio::broadcast`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    /// The raw text of the record, exactly as captured from the process.
    pub raw: String,
    /// How the record was parsed.
    pub parsed: ParsedContent,
    /// Which task/command produced this entry. Tasks and processes share an
    /// ID namespace — see `docs/runtime_engine_design.md` § Logging.
    pub source: TaskId,
    /// Sequence number (monotonic within a source).
    pub seq: u64,
    /// When this entry was received/created (wall clock).
    /// Always populated — use as fallback when `timestamp` is None.
    pub received_at: chrono::DateTime<Utc>,

    // Well-known fields (populated by FieldExtractor, all optional)
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,

    /// Additional extracted fields.
    pub fields: HashMap<String, serde_json::Value>,

    /// Which output stream this entry came from (stdout or stderr).
    /// `None` for entries not from a process stream (e.g., tracing events).
    pub stream: Option<Stream>,
}

impl LogEntry {
    /// Create a new LogEntry with `received_at` set to now.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        raw: String,
        parsed: ParsedContent,
        source: TaskId,
        seq: u64,
        timestamp: Option<String>,
        level: Option<String>,
        message: Option<String>,
        fields: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            raw,
            parsed,
            source,
            seq,
            received_at: Utc::now(),
            timestamp,
            level,
            message,
            fields,
            stream: None,
        }
    }

    /// Create a raw, undecorated log entry (no timestamp, level, or fields).
    ///
    /// Used by `TaskContext::println()` for plain text output that should
    /// appear without log decoration in any UI mode.
    pub fn raw(text: &str, source: TaskId) -> Self {
        Self {
            raw: text.to_string(),
            parsed: ParsedContent::PlainText,
            source,
            seq: 0,
            received_at: Utc::now(),
            timestamp: None,
            level: None,
            message: Some(text.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    /// Get the best available timestamp string for display.
    /// Prefers the extracted timestamp from log content; falls back to received_at.
    /// ISO 8601 timestamps are parsed and normalized to HH:MM:SS.mmm local time.
    pub fn display_timestamp(&self) -> String {
        if let Some(ts) = &self.timestamp {
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
                dt.with_timezone(&Local).format("%H:%M:%S%.3f").to_string()
            } else {
                ts.clone()
            }
        } else {
            self.received_at.format("%H:%M:%S%.3f").to_string()
        }
    }

    /// Get the raw string representation (for compatibility with former LogLine::as_str).
    pub fn as_str(&self) -> String {
        self.raw.clone()
    }
}
