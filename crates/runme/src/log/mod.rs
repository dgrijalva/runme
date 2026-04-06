pub mod parse;
pub mod extract;
pub mod filter;
pub mod search;
pub mod store;

use std::collections::HashMap;

/// Result of attempting to parse a line of input.
pub enum ParseResult {
    /// Successfully parsed a complete record.
    Record(RawRecord),
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
#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
pub struct LogEntry {
    /// The raw text of the record, exactly as captured from the process.
    pub raw: String,
    /// How the record was parsed.
    pub parsed: ParsedContent,
    /// Which task/command produced this entry.
    pub source: String,
    /// Sequence number (monotonic within a source).
    pub seq: u64,

    // Well-known fields (populated by FieldExtractor, all optional)
    pub timestamp: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,

    /// Additional extracted fields.
    pub fields: HashMap<String, serde_json::Value>,
}

impl LogEntry {
    /// Get the raw string representation (for compatibility with former LogLine::as_str).
    pub fn as_str(&self) -> String {
        self.raw.clone()
    }
}
