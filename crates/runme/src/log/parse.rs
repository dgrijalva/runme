use regex::Regex;

use super::{ParseResult, ParsedContent, RawRecord};

/// Fuses record splitting and parsing. Parsers may be stateful (buffering
/// partial records for multiline formats).
pub trait RecordParser: Send + Sync {
    /// Feed a line (or chunk) of text. Returns a parse result.
    fn feed(&mut self, line: &str) -> ParseResult;

    /// Flush any buffered partial input (e.g., at stream end).
    /// Returns a record if there was buffered content, None otherwise.
    fn flush(&mut self) -> Option<RawRecord>;

    /// Reset parser state (e.g., between commands).
    fn reset(&mut self);
}

// ---------------------------------------------------------------------------
// FallbackParser
// ---------------------------------------------------------------------------

/// Priority-ordered fallback parser. Tries each inner parser in order.
/// First `Record` wins. `Incomplete` means "buffer more and keep trying
/// this parser." `Rejection` means "try the next parser."
pub struct FallbackParser {
    parsers: Vec<Box<dyn RecordParser>>,
    /// Index of the parser that returned Incomplete last time (sticky).
    active: Option<usize>,
}

impl FallbackParser {
    pub fn new(parsers: Vec<Box<dyn RecordParser>>) -> Self {
        Self {
            parsers,
            active: None,
        }
    }
}

impl RecordParser for FallbackParser {
    fn feed(&mut self, line: &str) -> ParseResult {
        // If a parser previously returned Incomplete, feed to that one first.
        if let Some(idx) = self.active {
            match self.parsers[idx].feed(line) {
                ParseResult::Record(rec) => {
                    self.active = None;
                    return ParseResult::Record(rec);
                }
                ParseResult::Incomplete => {
                    return ParseResult::Incomplete;
                }
                ParseResult::Rejection => {
                    // The active parser gave up. Flush it and fall through.
                    self.active = None;
                    // Flush the previously-incomplete parser; if it produces
                    // a record we still need to try all parsers on *this* line.
                    let _ = self.parsers[idx].flush();
                }
            }
        }

        // Try each parser in priority order.
        for (i, parser) in self.parsers.iter_mut().enumerate() {
            match parser.feed(line) {
                ParseResult::Record(rec) => {
                    return ParseResult::Record(rec);
                }
                ParseResult::Incomplete => {
                    self.active = Some(i);
                    return ParseResult::Incomplete;
                }
                ParseResult::Rejection => {
                    continue;
                }
            }
        }

        // All rejected — should not happen if PlainLineParser is terminal.
        ParseResult::Rejection
    }

    fn flush(&mut self) -> Option<RawRecord> {
        if let Some(idx) = self.active.take() {
            return self.parsers[idx].flush();
        }
        for parser in &mut self.parsers {
            if let Some(rec) = parser.flush() {
                return Some(rec);
            }
        }
        None
    }

    fn reset(&mut self) {
        self.active = None;
        for parser in &mut self.parsers {
            parser.reset();
        }
    }
}

// ---------------------------------------------------------------------------
// JsonlParser
// ---------------------------------------------------------------------------

/// Detects JSON objects and arrays. Single-line by default. Tracks how many
/// JSON lines have been seen; after 3+ JSON lines, non-JSON input is flagged
/// as anomalous.
pub struct JsonlParser {
    json_line_count: u64,
}

impl JsonlParser {
    pub fn new() -> Self {
        Self { json_line_count: 0 }
    }
}

impl Default for JsonlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for JsonlParser {
    fn feed(&mut self, line: &str) -> ParseResult {
        let trimmed = line.trim();
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(val) if val.is_object() || val.is_array() => {
                self.json_line_count += 1;
                ParseResult::Record(RawRecord {
                    raw: line.to_string(),
                    parsed: ParsedContent::Json(val),
                })
            }
            _ => {
                // Non-JSON line after 3+ JSON lines → anomaly (but still a rejection
                // for this parser, so FallbackParser will try the next one; the anomaly
                // flag is communicated via a special JSON record).
                if self.json_line_count >= 3 {
                    let mut anomaly_fields = serde_json::Map::new();
                    anomaly_fields.insert(
                        "_anomalous".to_string(),
                        serde_json::Value::Bool(true),
                    );
                    anomaly_fields.insert(
                        "_anomaly_reason".to_string(),
                        serde_json::Value::String("plain_text_in_json_stream".to_string()),
                    );
                    anomaly_fields.insert(
                        "text".to_string(),
                        serde_json::Value::String(line.to_string()),
                    );
                    return ParseResult::Record(RawRecord {
                        raw: line.to_string(),
                        parsed: ParsedContent::Json(serde_json::Value::Object(anomaly_fields)),
                    });
                }
                ParseResult::Rejection
            }
        }
    }

    fn flush(&mut self) -> Option<RawRecord> {
        None
    }

    fn reset(&mut self) {
        self.json_line_count = 0;
    }
}

// ---------------------------------------------------------------------------
// RustPanicParser
// ---------------------------------------------------------------------------

/// Recognizes Rust panic output and captures the full backtrace as one record.
pub struct RustPanicParser {
    start_re: Regex,
    continuation_re: Regex,
    buffer: Vec<String>,
}

impl RustPanicParser {
    pub fn new() -> Self {
        Self {
            start_re: Regex::new(r"^thread\s+'[^']*'\s+panicked\s+at\s+").unwrap(),
            continuation_re: Regex::new(
                r"^(stack backtrace:|note:\s|\s+\d+:\s|\s+at\s)",
            )
            .unwrap(),
            buffer: Vec::new(),
        }
    }
}

impl Default for RustPanicParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for RustPanicParser {
    fn feed(&mut self, line: &str) -> ParseResult {
        if self.buffer.is_empty() {
            // Not currently buffering — check for start pattern.
            if self.start_re.is_match(line) {
                self.buffer.push(line.to_string());
                ParseResult::Incomplete
            } else {
                ParseResult::Rejection
            }
        } else {
            // Currently buffering — check for continuation.
            if self.continuation_re.is_match(line) {
                self.buffer.push(line.to_string());
                ParseResult::Incomplete
            } else {
                // End of panic record: non-continuation line encountered.
                // Emit the buffered record. The current non-matching line
                // is consumed — acceptable for wave 1. A pending-line
                // mechanism could preserve it but adds complexity.
                let raw = self.buffer.join("\n");
                self.buffer.clear();
                ParseResult::Record(RawRecord {
                    raw,
                    parsed: ParsedContent::PlainText,
                })
            }
        }
    }

    fn flush(&mut self) -> Option<RawRecord> {
        if self.buffer.is_empty() {
            None
        } else {
            let raw = self.buffer.join("\n");
            self.buffer.clear();
            Some(RawRecord {
                raw,
                parsed: ParsedContent::PlainText,
            })
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
    }
}

// ---------------------------------------------------------------------------
// CargoDiagnosticParser
// ---------------------------------------------------------------------------

/// Recognizes cargo compiler errors/warnings and captures the full diagnostic
/// as one record.
pub struct CargoDiagnosticParser {
    start_re: Regex,
    continuation_re: Regex,
    buffer: Vec<String>,
}

impl CargoDiagnosticParser {
    pub fn new() -> Self {
        Self {
            start_re: Regex::new(r"^(error|warning)(\[E\d{4}\])?:\s").unwrap(),
            continuation_re: Regex::new(
                r"^(\s*-->|\s*\||\s*=\s*(note|help|warning):|\s*$)",
            )
            .unwrap(),
            buffer: Vec::new(),
        }
    }
}

impl Default for CargoDiagnosticParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for CargoDiagnosticParser {
    fn feed(&mut self, line: &str) -> ParseResult {
        if self.buffer.is_empty() {
            if self.start_re.is_match(line) {
                self.buffer.push(line.to_string());
                ParseResult::Incomplete
            } else {
                ParseResult::Rejection
            }
        } else {
            // Check if this line starts a NEW diagnostic (end of current one).
            if self.start_re.is_match(line) {
                let raw = self.buffer.join("\n");
                self.buffer.clear();
                // Start buffering the new diagnostic.
                self.buffer.push(line.to_string());
                ParseResult::Record(RawRecord {
                    raw,
                    parsed: ParsedContent::PlainText,
                })
            } else if self.continuation_re.is_match(line) {
                self.buffer.push(line.to_string());
                ParseResult::Incomplete
            } else {
                // Non-matching, non-start line: end of diagnostic.
                let raw = self.buffer.join("\n");
                self.buffer.clear();
                ParseResult::Record(RawRecord {
                    raw,
                    parsed: ParsedContent::PlainText,
                })
            }
        }
    }

    fn flush(&mut self) -> Option<RawRecord> {
        if self.buffer.is_empty() {
            None
        } else {
            let raw = self.buffer.join("\n");
            self.buffer.clear();
            Some(RawRecord {
                raw,
                parsed: ParsedContent::PlainText,
            })
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
    }
}

// ---------------------------------------------------------------------------
// LogfmtParser
// ---------------------------------------------------------------------------

/// Parses `key=value` format (logfmt). Split on spaces, split on `=`,
/// handle quoted values.
pub struct LogfmtParser;

impl LogfmtParser {
    pub fn new() -> Self {
        Self
    }

    /// Parse a logfmt line into key-value pairs. Returns None if the line
    /// doesn't look like logfmt (no `key=value` pairs found).
    fn parse_logfmt(line: &str) -> Option<Vec<(String, String)>> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let mut pairs = Vec::new();
        let mut chars = trimmed.chars().peekable();

        while chars.peek().is_some() {
            // Skip whitespace
            while chars.peek() == Some(&' ') {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }

            // Read key (up to '=')
            let mut key = String::new();
            loop {
                match chars.peek() {
                    Some(&'=') => {
                        chars.next(); // consume '='
                        break;
                    }
                    Some(&' ') | None => {
                        // Bare word without '=' — not logfmt
                        return None;
                    }
                    Some(&c) => {
                        key.push(c);
                        chars.next();
                    }
                }
            }

            if key.is_empty() {
                return None;
            }

            // Read value
            let value = match chars.peek() {
                Some(&'"') => {
                    // Quoted value
                    chars.next(); // consume opening quote
                    let mut val = String::new();
                    let mut escaped = false;
                    loop {
                        match chars.next() {
                            Some('\\') if !escaped => {
                                escaped = true;
                            }
                            Some('"') if !escaped => {
                                break;
                            }
                            Some(c) => {
                                if escaped {
                                    val.push('\\');
                                    escaped = false;
                                }
                                val.push(c);
                            }
                            None => {
                                // Unterminated quote — treat as not logfmt
                                return None;
                            }
                        }
                    }
                    val
                }
                Some(&' ') | None => {
                    // Empty value
                    String::new()
                }
                _ => {
                    // Unquoted value (up to next space)
                    let mut val = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == ' ' {
                            break;
                        }
                        val.push(c);
                        chars.next();
                    }
                    val
                }
            };

            pairs.push((key, value));
        }

        // Need at least 1 key=value pair to be considered logfmt
        if pairs.is_empty() {
            None
        } else {
            Some(pairs)
        }
    }
}

impl Default for LogfmtParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for LogfmtParser {
    fn feed(&mut self, line: &str) -> ParseResult {
        match Self::parse_logfmt(line) {
            Some(pairs) => ParseResult::Record(RawRecord {
                raw: line.to_string(),
                parsed: ParsedContent::Logfmt(pairs),
            }),
            None => ParseResult::Rejection,
        }
    }

    fn flush(&mut self) -> Option<RawRecord> {
        None
    }

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// PlainLineParser
// ---------------------------------------------------------------------------

/// Always succeeds. One line = one record. Terminal fallback.
pub struct PlainLineParser;

impl RecordParser for PlainLineParser {
    fn feed(&mut self, line: &str) -> ParseResult {
        ParseResult::Record(RawRecord {
            raw: line.to_string(),
            parsed: ParsedContent::PlainText,
        })
    }

    fn flush(&mut self) -> Option<RawRecord> {
        None
    }

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// Default parser chain constructor
// ---------------------------------------------------------------------------

/// Build the default parser chain as specified in the design doc.
pub fn default_parser() -> FallbackParser {
    FallbackParser::new(vec![
        Box::new(JsonlParser::new()),
        Box::new(RustPanicParser::new()),
        Box::new(CargoDiagnosticParser::new()),
        Box::new(LogfmtParser::new()),
        Box::new(PlainLineParser),
    ])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- JsonlParser --

    #[test]
    fn jsonl_parses_object() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(r#"{"level":"info","message":"hello"}"#);
        match result {
            ParseResult::Record(rec) => {
                assert!(matches!(rec.parsed, ParsedContent::Json(_)));
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["level"], "info");
                }
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn jsonl_parses_array() {
        let mut parser = JsonlParser::new();
        let result = parser.feed("[1, 2, 3]");
        assert!(matches!(result, ParseResult::Record(_)));
    }

    #[test]
    fn jsonl_rejects_plain_text() {
        let mut parser = JsonlParser::new();
        let result = parser.feed("just plain text");
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_rejects_json_string() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(r#""just a string""#);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_rejects_number() {
        let mut parser = JsonlParser::new();
        let result = parser.feed("42");
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_anomaly_detection() {
        let mut parser = JsonlParser::new();
        // Feed 3 JSON lines
        parser.feed(r#"{"a":1}"#);
        parser.feed(r#"{"b":2}"#);
        parser.feed(r#"{"c":3}"#);

        // Now a non-JSON line should be flagged as anomalous
        let result = parser.feed("plain text after json");
        match result {
            ParseResult::Record(rec) => {
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["_anomalous"], true);
                    assert_eq!(val["_anomaly_reason"], "plain_text_in_json_stream");
                } else {
                    panic!("expected Json parsed content");
                }
            }
            _ => panic!("expected Record for anomaly"),
        }
    }

    #[test]
    fn jsonl_no_anomaly_before_threshold() {
        let mut parser = JsonlParser::new();
        // Only 2 JSON lines
        parser.feed(r#"{"a":1}"#);
        parser.feed(r#"{"b":2}"#);
        // Non-JSON line should be a Rejection, not anomaly
        let result = parser.feed("plain text");
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_reset_clears_count() {
        let mut parser = JsonlParser::new();
        parser.feed(r#"{"a":1}"#);
        parser.feed(r#"{"b":2}"#);
        parser.feed(r#"{"c":3}"#);
        parser.reset();
        // After reset, non-JSON should be Rejection (count was cleared)
        let result = parser.feed("plain text");
        assert!(matches!(result, ParseResult::Rejection));
    }

    // -- RustPanicParser --

    #[test]
    fn rust_panic_detects_start() {
        let mut parser = RustPanicParser::new();
        let result = parser.feed("thread 'main' panicked at 'index out of bounds', src/main.rs:10:5");
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn rust_panic_captures_backtrace() {
        let mut parser = RustPanicParser::new();
        parser.feed("thread 'main' panicked at 'error', src/main.rs:10:5");
        parser.feed("stack backtrace:");
        parser.feed("   0: std::panicking::begin_panic");
        parser.feed("   1: myapp::main");

        // Flush should produce the record
        let rec = parser.flush().expect("should have a record");
        assert!(rec.raw.contains("thread 'main' panicked"));
        assert!(rec.raw.contains("stack backtrace:"));
        assert!(rec.raw.contains("myapp::main"));
    }

    #[test]
    fn rust_panic_ends_on_non_matching() {
        let mut parser = RustPanicParser::new();
        parser.feed("thread 'main' panicked at 'error', src/main.rs:10:5");
        parser.feed("stack backtrace:");
        let result = parser.feed("some other output");
        match result {
            ParseResult::Record(rec) => {
                assert!(rec.raw.contains("thread 'main' panicked"));
                assert!(rec.raw.contains("stack backtrace:"));
                assert!(!rec.raw.contains("some other output"));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn rust_panic_rejects_non_panic() {
        let mut parser = RustPanicParser::new();
        let result = parser.feed("normal output line");
        assert!(matches!(result, ParseResult::Rejection));
    }

    // -- CargoDiagnosticParser --

    #[test]
    fn cargo_diag_detects_error() {
        let mut parser = CargoDiagnosticParser::new();
        let result = parser.feed("error[E0308]: mismatched types");
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn cargo_diag_detects_warning() {
        let mut parser = CargoDiagnosticParser::new();
        let result = parser.feed("warning: unused variable");
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn cargo_diag_captures_full_diagnostic() {
        let mut parser = CargoDiagnosticParser::new();
        parser.feed("error[E0308]: mismatched types");
        parser.feed("  --> src/main.rs:5:10");
        parser.feed("   |");
        parser.feed("   |     let x: i32 = \"hello\";");
        parser.feed("   |                  ^^^^^^^ expected i32, found &str");

        let rec = parser.flush().expect("should have a record");
        assert!(rec.raw.contains("error[E0308]"));
        assert!(rec.raw.contains("--> src/main.rs"));
    }

    #[test]
    fn cargo_diag_ends_on_new_diagnostic() {
        let mut parser = CargoDiagnosticParser::new();
        parser.feed("error[E0308]: mismatched types");
        parser.feed("  --> src/main.rs:5:10");
        // New diagnostic starts
        let result = parser.feed("warning: unused variable");
        match result {
            ParseResult::Record(rec) => {
                assert!(rec.raw.contains("error[E0308]"));
                assert!(!rec.raw.contains("warning:"));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn cargo_diag_rejects_non_diagnostic() {
        let mut parser = CargoDiagnosticParser::new();
        let result = parser.feed("Compiling myapp v0.1.0");
        assert!(matches!(result, ParseResult::Rejection));
    }

    // -- LogfmtParser --

    #[test]
    fn logfmt_parses_basic() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed("level=info msg=\"hello world\" duration=1.23");
        match result {
            ParseResult::Record(rec) => {
                if let ParsedContent::Logfmt(pairs) = &rec.parsed {
                    assert_eq!(pairs.len(), 3);
                    assert_eq!(pairs[0], ("level".to_string(), "info".to_string()));
                    assert_eq!(pairs[1], ("msg".to_string(), "hello world".to_string()));
                    assert_eq!(pairs[2], ("duration".to_string(), "1.23".to_string()));
                } else {
                    panic!("expected Logfmt");
                }
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn logfmt_rejects_plain_text() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed("just plain text without equals");
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn logfmt_empty_value() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed("key= other=val");
        match result {
            ParseResult::Record(rec) => {
                if let ParsedContent::Logfmt(pairs) = &rec.parsed {
                    assert_eq!(pairs[0], ("key".to_string(), String::new()));
                    assert_eq!(pairs[1], ("other".to_string(), "val".to_string()));
                } else {
                    panic!("expected Logfmt");
                }
            }
            _ => panic!("expected Record"),
        }
    }

    // -- PlainLineParser --

    #[test]
    fn plain_always_succeeds() {
        let mut parser = PlainLineParser;
        let result = parser.feed("anything at all");
        match result {
            ParseResult::Record(rec) => {
                assert_eq!(rec.raw, "anything at all");
                assert!(matches!(rec.parsed, ParsedContent::PlainText));
            }
            _ => panic!("expected Record"),
        }
    }

    // -- FallbackParser --

    #[test]
    fn fallback_json_wins_over_plain() {
        let mut parser = default_parser();
        let result = parser.feed(r#"{"level":"info"}"#);
        match result {
            ParseResult::Record(rec) => {
                assert!(matches!(rec.parsed, ParsedContent::Json(_)));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_plain_text_fallback() {
        let mut parser = default_parser();
        let result = parser.feed("just plain text");
        match result {
            ParseResult::Record(rec) => {
                assert!(matches!(rec.parsed, ParsedContent::PlainText));
                assert_eq!(rec.raw, "just plain text");
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_logfmt_detected() {
        let mut parser = default_parser();
        let result = parser.feed("level=info msg=hello");
        match result {
            ParseResult::Record(rec) => {
                assert!(matches!(rec.parsed, ParsedContent::Logfmt(_)));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_rust_panic_multiline() {
        let mut parser = default_parser();
        let r1 = parser.feed("thread 'main' panicked at 'oh no', src/main.rs:1:1");
        assert!(matches!(r1, ParseResult::Incomplete));
        let r2 = parser.feed("stack backtrace:");
        assert!(matches!(r2, ParseResult::Incomplete));

        // Non-continuation line triggers record emission
        let r3 = parser.feed("some other line");
        match r3 {
            ParseResult::Record(rec) => {
                assert!(rec.raw.contains("thread 'main' panicked"));
            }
            _ => panic!("expected Record from panic flush"),
        }
    }

    // -- Autodetection end-to-end --

    #[test]
    fn autodetect_json_then_plain() {
        let mut parser = default_parser();

        // JSON auto-detected
        let r1 = parser.feed(r#"{"msg":"hello"}"#);
        assert!(matches!(r1, ParseResult::Record(RawRecord { parsed: ParsedContent::Json(_), .. })));

        // Plain text fallback
        let r2 = parser.feed("plain line");
        assert!(matches!(r2, ParseResult::Record(RawRecord { parsed: ParsedContent::PlainText, .. })));
    }

    #[test]
    fn autodetect_anomaly_in_json_stream() {
        let mut parser = default_parser();

        // 3 JSON lines
        parser.feed(r#"{"a":1}"#);
        parser.feed(r#"{"b":2}"#);
        parser.feed(r#"{"c":3}"#);

        // Non-JSON line should be flagged as anomalous
        let result = parser.feed("PANIC: something went wrong");
        match result {
            ParseResult::Record(rec) => {
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["_anomalous"], true);
                } else {
                    panic!("expected Json with anomaly flag");
                }
            }
            _ => panic!("expected Record"),
        }
    }
}
