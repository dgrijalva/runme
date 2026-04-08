pub mod buffer;
pub mod extract;
pub mod filter;
pub mod format;
pub mod parse;
pub mod search;
pub mod store;
pub mod stream;

use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;

/// Which output stream a log entry originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
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
#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    /// The raw text of the record, exactly as captured from the process.
    pub raw: String,
    /// How the record was parsed.
    pub parsed: ParsedContent,
    /// Which task/command produced this entry.
    pub source: String,
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
        source: String,
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
    pub fn raw(text: &str, source: &str) -> Self {
        Self {
            raw: text.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
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
    pub fn display_timestamp(&self) -> String {
        if let Some(ts) = &self.timestamp {
            ts.clone()
        } else {
            self.received_at.format("%H:%M:%S%.3f").to_string()
        }
    }

    /// Get the raw string representation (for compatibility with former LogLine::as_str).
    pub fn as_str(&self) -> String {
        self.raw.clone()
    }
}
