use regex::Regex;

use super::{ParseResult, ParsedContent, RawRecord};

/// Fuses record splitting and parsing. Operates on raw bytes.
///
/// Parsers do **not** buffer data internally -- the caller owns the buffer
/// and re-feeds the full unconsumed slice on each call.
pub trait RecordParser: Send + Sync {
    /// Scan the front of `data` for a record.
    ///
    /// - `data`: the accumulated bytes not yet consumed. The parser examines
    ///   the beginning of this slice and returns how many bytes it consumed.
    /// - `eof`: true when no more data will arrive (process exited). Parsers
    ///   that would normally return `Incomplete` should emit what they have
    ///   or `Rejection` so the next parser can try.
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult;

    /// Reset parser state (e.g., between commands).
    fn reset(&mut self);
}

// ---------------------------------------------------------------------------
// FallbackParser
// ---------------------------------------------------------------------------

/// Priority-ordered fallback parser. Tries each inner parser in order.
/// First `Record` wins. `Incomplete` means "buffer more and keep trying
/// this parser." `Rejection` means "try the next parser."
///
/// Tracks an "active parser" index. When a parser returns `Incomplete`,
/// it becomes active and is tried exclusively on subsequent calls until
/// it either produces a `Record` (active cleared, restart from top)
/// or `Rejection` (active cleared, try next parser).
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
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        // If a parser previously returned Incomplete, feed to that one first.
        if let Some(idx) = self.active {
            match self.parsers[idx].feed(data, eof) {
                ParseResult::Record(rec, n) => {
                    self.active = None;
                    return ParseResult::Record(rec, n);
                }
                ParseResult::Incomplete => {
                    return ParseResult::Incomplete;
                }
                ParseResult::Rejection => {
                    // The active parser gave up. Clear it and fall through.
                    self.active = None;
                    // Continue from the next parser below.
                }
            }
        }

        let start = self.active.map(|i| i + 1).unwrap_or(0);
        self.active = None;

        for i in start..self.parsers.len() {
            match self.parsers[i].feed(data, eof) {
                ParseResult::Record(rec, n) => {
                    return ParseResult::Record(rec, n);
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

        // All rejected -- should not happen if PlainLineParser is terminal.
        ParseResult::Rejection
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

/// Detects JSON objects and arrays from raw bytes. Uses brace/bracket depth
/// tracking (skipping quoted strings) to find the end of a JSON record.
/// Handles both newline-delimited JSON and concatenated JSON (`{"a":1}{"b":2}`).
///
/// Anomaly detection: after 3+ JSON records, non-JSON input is rejected
/// (not emitted as a record -- the next parser handles it).
pub struct JsonlParser {
    json_record_count: u64,
}

impl JsonlParser {
    pub fn new() -> Self {
        Self { json_record_count: 0 }
    }

    /// Skip leading whitespace/newlines in the byte slice.
    /// Returns the number of bytes skipped.
    fn skip_whitespace(data: &[u8]) -> usize {
        let mut i = 0;
        while i < data.len() && (data[i] == b' ' || data[i] == b'\t' || data[i] == b'\n' || data[i] == b'\r') {
            i += 1;
        }
        i
    }

    /// Find the end of a JSON value starting at `data[0]` which must be `{` or `[`.
    /// Returns the byte index past the closing delimiter, or None if incomplete.
    /// Tracks brace/bracket depth, skipping over quoted strings.
    fn find_json_end(data: &[u8]) -> Option<usize> {
        if data.is_empty() {
            return None;
        }

        let open = data[0];
        let close = match open {
            b'{' => b'}',
            b'[' => b']',
            _ => return None,
        };

        let mut depth: usize = 0;
        let mut i: usize = 0;
        let mut in_string = false;
        let mut escape = false;

        while i < data.len() {
            let b = data[i];

            if escape {
                escape = false;
                i += 1;
                continue;
            }

            if in_string {
                match b {
                    b'\\' => escape = true,
                    b'"' => in_string = false,
                    _ => {}
                }
                i += 1;
                continue;
            }

            match b {
                b'"' => in_string = true,
                b if b == open => depth += 1,
                b if b == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }

        None // incomplete
    }

    /// Count trailing whitespace/newlines after a JSON value for consumption.
    fn trailing_whitespace(data: &[u8]) -> usize {
        let mut i = 0;
        while i < data.len() && (data[i] == b' ' || data[i] == b'\t' || data[i] == b'\n' || data[i] == b'\r') {
            i += 1;
        }
        i
    }
}

impl Default for JsonlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for JsonlParser {
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        if data.is_empty() {
            return if eof { ParseResult::Rejection } else { ParseResult::Incomplete };
        }

        let ws_skip = Self::skip_whitespace(data);
        if ws_skip >= data.len() {
            // All whitespace
            return if eof { ParseResult::Rejection } else { ParseResult::Incomplete };
        }

        let first_byte = data[ws_skip];

        // Check if it starts with { or [
        if first_byte != b'{' && first_byte != b'[' {
            // Not JSON. If we've seen 3+ JSON records, reject (anomaly detection --
            // the next parser will handle it).
            if self.json_record_count > 3 {
                return ParseResult::Rejection;
            }
            return ParseResult::Rejection;
        }

        // Try to find the end of the JSON value
        match Self::find_json_end(&data[ws_skip..]) {
            Some(json_len) => {
                let json_bytes = &data[ws_skip..ws_skip + json_len];

                // Try to parse as JSON
                match serde_json::from_slice::<serde_json::Value>(json_bytes) {
                    Ok(val) if val.is_object() || val.is_array() => {
                        self.json_record_count += 1;
                        let raw = String::from_utf8_lossy(json_bytes).into_owned();

                        // Count trailing whitespace after JSON
                        let trailing = Self::trailing_whitespace(&data[ws_skip + json_len..]);
                        let consumed = ws_skip + json_len + trailing;

                        ParseResult::Record(
                            RawRecord {
                                raw,
                                parsed: ParsedContent::Json(val),
                            },
                            consumed,
                        )
                    }
                    _ => {
                        // Valid JSON structure but not object/array, or parse error
                        ParseResult::Rejection
                    }
                }
            }
            None => {
                // Incomplete JSON structure
                if eof {
                    // At EOF with incomplete JSON -- emit as PlainText
                    let raw = String::from_utf8_lossy(&data[ws_skip..]).into_owned();
                    let consumed = data.len();
                    ParseResult::Record(
                        RawRecord {
                            raw,
                            parsed: ParsedContent::PlainText,
                        },
                        consumed,
                    )
                } else {
                    ParseResult::Incomplete
                }
            }
        }
    }

    fn reset(&mut self) {
        self.json_record_count = 0;
    }
}

// ---------------------------------------------------------------------------
// RustPanicParser
// ---------------------------------------------------------------------------

/// Recognizes Rust panic output and captures the full backtrace as one record.
/// Operates on raw bytes, converting to string (lossy) for regex matching.
pub struct RustPanicParser {
    start_re: Regex,
    continuation_re: Regex,
}

impl RustPanicParser {
    pub fn new() -> Self {
        Self {
            start_re: Regex::new(r"^thread\s+'[^']*'\s+panicked\s+at\s+").unwrap(),
            continuation_re: Regex::new(
                r"^(stack backtrace:|note:\s|\s+\d+:\s|\s+at\s)",
            )
            .unwrap(),
        }
    }
}

impl Default for RustPanicParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for RustPanicParser {
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        if data.is_empty() {
            return if eof { ParseResult::Rejection } else { ParseResult::Incomplete };
        }

        let text = String::from_utf8_lossy(data);

        // Check if the first line matches the start pattern
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let first_line = &text[..first_line_end];

        if !self.start_re.is_match(first_line) {
            return ParseResult::Rejection;
        }

        // We have a panic start. Scan line by line for continuation patterns.
        let mut consumed_lines_end = first_line_end;
        // Advance past the first newline if present
        if consumed_lines_end < text.len() && text.as_bytes()[consumed_lines_end] == b'\n' {
            consumed_lines_end += 1;
        }

        loop {
            if consumed_lines_end >= text.len() {
                // We've consumed all the data
                if eof {
                    // Emit what we have
                    let raw = text[..consumed_lines_end].trim_end_matches('\n').to_string();
                    return ParseResult::Record(
                        RawRecord {
                            raw,
                            parsed: ParsedContent::PlainText,
                        },
                        data.len(),
                    );
                } else {
                    return ParseResult::Incomplete;
                }
            }

            // Find the next line
            let remaining = &text[consumed_lines_end..];
            let next_line_end = remaining.find('\n').unwrap_or(remaining.len());
            let next_line = &remaining[..next_line_end];

            if self.continuation_re.is_match(next_line) {
                consumed_lines_end += next_line_end;
                // Advance past the newline if present
                if consumed_lines_end < text.len() && text.as_bytes()[consumed_lines_end] == b'\n' {
                    consumed_lines_end += 1;
                }
            } else {
                if next_line_end == remaining.len() && !eof {
                    // The next line has no newline and we're not at eof --
                    // it could still be a continuation line that hasn't arrived fully.
                    // But if the line is non-empty, we should decide now.
                    if next_line.is_empty() {
                        return ParseResult::Incomplete;
                    }
                }
                // Non-continuation line found; emit the panic record.
                // consumed_lines_end points to the start of the non-continuation line.
                let raw = text[..consumed_lines_end].trim_end_matches('\n').to_string();
                // consumed bytes = bytes up to start of non-matching line
                let consumed_bytes = consumed_lines_end.min(data.len());
                return ParseResult::Record(
                    RawRecord {
                        raw,
                        parsed: ParsedContent::PlainText,
                    },
                    consumed_bytes,
                );
            }
        }
    }

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// CargoDiagnosticParser
// ---------------------------------------------------------------------------

/// Recognizes cargo compiler errors/warnings and captures the full diagnostic
/// as one record. Operates on raw bytes.
pub struct CargoDiagnosticParser {
    start_re: Regex,
    continuation_re: Regex,
}

impl CargoDiagnosticParser {
    pub fn new() -> Self {
        Self {
            start_re: Regex::new(r"^(error|warning)(\[E\d{4}\])?:\s").unwrap(),
            continuation_re: Regex::new(
                r"^(\s*-->|\s*\||\s*=\s*(note|help|warning):|\s*$)",
            )
            .unwrap(),
        }
    }
}

impl Default for CargoDiagnosticParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordParser for CargoDiagnosticParser {
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        if data.is_empty() {
            return if eof { ParseResult::Rejection } else { ParseResult::Incomplete };
        }

        let text = String::from_utf8_lossy(data);

        // Check if the first line matches the start pattern
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let first_line = &text[..first_line_end];

        if !self.start_re.is_match(first_line) {
            return ParseResult::Rejection;
        }

        // We have a diagnostic start. Scan line by line for continuation patterns.
        let mut consumed_lines_end = first_line_end;
        // Advance past the first newline if present
        if consumed_lines_end < text.len() && text.as_bytes()[consumed_lines_end] == b'\n' {
            consumed_lines_end += 1;
        }

        loop {
            if consumed_lines_end >= text.len() {
                if eof {
                    let raw = text[..consumed_lines_end].trim_end_matches('\n').to_string();
                    return ParseResult::Record(
                        RawRecord {
                            raw,
                            parsed: ParsedContent::PlainText,
                        },
                        data.len(),
                    );
                } else {
                    return ParseResult::Incomplete;
                }
            }

            let remaining = &text[consumed_lines_end..];
            let next_line_end = remaining.find('\n').unwrap_or(remaining.len());
            let next_line = &remaining[..next_line_end];

            // Check if this line starts a NEW diagnostic (end of current one)
            if self.start_re.is_match(next_line) {
                let raw = text[..consumed_lines_end].trim_end_matches('\n').to_string();
                let consumed_bytes = consumed_lines_end.min(data.len());
                return ParseResult::Record(
                    RawRecord {
                        raw,
                        parsed: ParsedContent::PlainText,
                    },
                    consumed_bytes,
                );
            }

            if self.continuation_re.is_match(next_line) {
                consumed_lines_end += next_line_end;
                if consumed_lines_end < text.len() && text.as_bytes()[consumed_lines_end] == b'\n' {
                    consumed_lines_end += 1;
                }
            } else {
                if next_line_end == remaining.len() && !eof && next_line.is_empty() {
                    return ParseResult::Incomplete;
                }
                // Non-matching, non-start line: end of diagnostic.
                let raw = text[..consumed_lines_end].trim_end_matches('\n').to_string();
                let consumed_bytes = consumed_lines_end.min(data.len());
                return ParseResult::Record(
                    RawRecord {
                        raw,
                        parsed: ParsedContent::PlainText,
                    },
                    consumed_bytes,
                );
            }
        }
    }

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// LogfmtParser
// ---------------------------------------------------------------------------

/// Parses `key=value` format (logfmt). Scans for a newline-terminated line
/// and parses it as logfmt key=value pairs.
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
                        // Bare word without '=' -- not logfmt
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
                                // Unterminated quote -- treat as not logfmt
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
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        if data.is_empty() {
            return if eof { ParseResult::Rejection } else { ParseResult::Incomplete };
        }

        // Look for a newline
        if let Some(pos) = data.iter().position(|&b| b == b'\n') {
            let line_bytes = &data[..pos];
            let line = String::from_utf8_lossy(line_bytes);
            match Self::parse_logfmt(&line) {
                Some(pairs) => ParseResult::Record(
                    RawRecord {
                        raw: line.into_owned(),
                        parsed: ParsedContent::Logfmt(pairs),
                    },
                    pos + 1, // consume the newline too
                ),
                None => ParseResult::Rejection,
            }
        } else if eof {
            // No newline but at EOF -- try the whole buffer
            let line = String::from_utf8_lossy(data);
            match Self::parse_logfmt(&line) {
                Some(pairs) => ParseResult::Record(
                    RawRecord {
                        raw: line.into_owned(),
                        parsed: ParsedContent::Logfmt(pairs),
                    },
                    data.len(),
                ),
                None => ParseResult::Rejection,
            }
        } else {
            // No newline yet and not EOF -- could be incomplete logfmt
            // Check if what we have so far could be logfmt to decide Incomplete vs Rejection
            let line = String::from_utf8_lossy(data);
            if line.contains('=') {
                ParseResult::Incomplete
            } else {
                ParseResult::Rejection
            }
        }
    }

    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// PlainLineParser
// ---------------------------------------------------------------------------

/// Scans for `\n`. Returns everything up to the newline as one record.
/// At EOF, emits whatever remains. Always succeeds -- terminal fallback.
pub struct PlainLineParser;

impl RecordParser for PlainLineParser {
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        if data.is_empty() {
            return if eof { ParseResult::Rejection } else { ParseResult::Incomplete };
        }

        if let Some(pos) = data.iter().position(|&b| b == b'\n') {
            // Found a newline -- emit everything before it
            let raw = String::from_utf8_lossy(&data[..pos]).into_owned();
            ParseResult::Record(
                RawRecord {
                    raw,
                    parsed: ParsedContent::PlainText,
                },
                pos + 1, // consume the newline
            )
        } else if eof {
            // No newline but at EOF -- emit remaining bytes
            let raw = String::from_utf8_lossy(data).into_owned();
            ParseResult::Record(
                RawRecord {
                    raw,
                    parsed: ParsedContent::PlainText,
                },
                data.len(),
            )
        } else {
            // No newline yet, not at EOF -- need more data
            ParseResult::Incomplete
        }
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
        let input = b"{\"level\":\"info\",\"message\":\"hello\"}\n";
        let result = parser.feed(input, false);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(matches!(rec.parsed, ParsedContent::Json(_)));
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["level"], "info");
                }
                assert_eq!(consumed, input.len());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn jsonl_parses_object_no_trailing_newline_eof() {
        let mut parser = JsonlParser::new();
        let input = b"{\"level\":\"info\"}";
        let result = parser.feed(input, true);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(matches!(rec.parsed, ParsedContent::Json(_)));
                assert_eq!(consumed, input.len());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn jsonl_parses_array() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(b"[1, 2, 3]\n", false);
        assert!(matches!(result, ParseResult::Record(_, _)));
    }

    #[test]
    fn jsonl_rejects_plain_text() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(b"just plain text\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_rejects_json_string() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(b"\"just a string\"\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_rejects_number() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(b"42\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_anomaly_detection() {
        let mut parser = JsonlParser::new();
        // Feed 4 JSON records to exceed threshold
        parser.feed(b"{\"a\":1}\n", false);
        parser.feed(b"{\"b\":2}\n", false);
        parser.feed(b"{\"c\":3}\n", false);
        parser.feed(b"{\"d\":4}\n", false);

        // Now a non-JSON line should be rejected (anomaly --
        // the next parser handles it)
        let result = parser.feed(b"plain text after json\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_no_anomaly_before_threshold() {
        let mut parser = JsonlParser::new();
        // Only 2 JSON records
        parser.feed(b"{\"a\":1}\n", false);
        parser.feed(b"{\"b\":2}\n", false);
        // Non-JSON line should be a Rejection
        let result = parser.feed(b"plain text\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_reset_clears_count() {
        let mut parser = JsonlParser::new();
        parser.feed(b"{\"a\":1}\n", false);
        parser.feed(b"{\"b\":2}\n", false);
        parser.feed(b"{\"c\":3}\n", false);
        parser.feed(b"{\"d\":4}\n", false);
        parser.reset();
        // After reset, non-JSON should be Rejection (count was cleared)
        let result = parser.feed(b"plain text\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn jsonl_concatenated_json() {
        let mut parser = JsonlParser::new();
        let input = b"{\"a\":1}{\"b\":2}";
        // First call should parse the first object
        let result = parser.feed(input, false);
        match result {
            ParseResult::Record(rec, consumed) => {
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["a"], 1);
                }
                // Should consume just the first JSON object
                assert_eq!(consumed, 7); // {"a":1}

                // Feed the remaining bytes
                let result2 = parser.feed(&input[consumed..], true);
                match result2 {
                    ParseResult::Record(rec2, consumed2) => {
                        if let ParsedContent::Json(val) = &rec2.parsed {
                            assert_eq!(val["b"], 2);
                        }
                        assert_eq!(consumed2, 7); // {"b":2}
                    }
                    _ => panic!("expected second Record"),
                }
            }
            _ => panic!("expected first Record"),
        }
    }

    #[test]
    fn jsonl_incomplete_then_complete() {
        let mut parser = JsonlParser::new();
        // Partial JSON
        let result = parser.feed(b"{\"a\":", false);
        assert!(matches!(result, ParseResult::Incomplete));

        // Now the full JSON
        let result = parser.feed(b"{\"a\":1}\n", false);
        match result {
            ParseResult::Record(rec, consumed) => {
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["a"], 1);
                }
                assert_eq!(consumed, 8); // {"a":1}\n
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn jsonl_eof_incomplete_emits_plaintext() {
        let mut parser = JsonlParser::new();
        let result = parser.feed(b"{\"a\":", true);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(matches!(rec.parsed, ParsedContent::PlainText));
                assert_eq!(consumed, 5);
            }
            _ => panic!("expected Record as PlainText at EOF"),
        }
    }

    #[test]
    fn jsonl_handles_strings_with_braces() {
        let mut parser = JsonlParser::new();
        let input = b"{\"msg\":\"{hello}\"}\n";
        let result = parser.feed(input, false);
        match result {
            ParseResult::Record(rec, consumed) => {
                if let ParsedContent::Json(val) = &rec.parsed {
                    assert_eq!(val["msg"], "{hello}");
                }
                assert_eq!(consumed, input.len());
            }
            _ => panic!("expected Record"),
        }
    }

    // -- RustPanicParser --

    #[test]
    fn rust_panic_detects_start() {
        let mut parser = RustPanicParser::new();
        let result = parser.feed(b"thread 'main' panicked at 'index out of bounds', src/main.rs:10:5\n", false);
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn rust_panic_captures_backtrace() {
        let mut parser = RustPanicParser::new();
        let input = b"thread 'main' panicked at 'error', src/main.rs:10:5\nstack backtrace:\n   0: std::panicking::begin_panic\n   1: myapp::main\n";
        let result = parser.feed(input, true);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(rec.raw.contains("thread 'main' panicked"));
                assert!(rec.raw.contains("stack backtrace:"));
                assert!(rec.raw.contains("myapp::main"));
                assert_eq!(consumed, input.len());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn rust_panic_ends_on_non_matching() {
        let mut parser = RustPanicParser::new();
        let input = b"thread 'main' panicked at 'error', src/main.rs:10:5\nstack backtrace:\nsome other output\n";
        let result = parser.feed(input, false);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(rec.raw.contains("thread 'main' panicked"));
                assert!(rec.raw.contains("stack backtrace:"));
                assert!(!rec.raw.contains("some other output"));
                // consumed should be up to the start of "some other output"
                assert!(consumed < input.len());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn rust_panic_rejects_non_panic() {
        let mut parser = RustPanicParser::new();
        let result = parser.feed(b"normal output line\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    // -- CargoDiagnosticParser --

    #[test]
    fn cargo_diag_detects_error() {
        let mut parser = CargoDiagnosticParser::new();
        let result = parser.feed(b"error[E0308]: mismatched types\n", false);
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn cargo_diag_detects_warning() {
        let mut parser = CargoDiagnosticParser::new();
        let result = parser.feed(b"warning: unused variable\n", false);
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn cargo_diag_captures_full_diagnostic() {
        let mut parser = CargoDiagnosticParser::new();
        let input = b"error[E0308]: mismatched types\n  --> src/main.rs:5:10\n   |\n   |     let x: i32 = \"hello\";\n   |                  ^^^^^^^ expected i32, found &str\n";
        let result = parser.feed(input, true);
        match result {
            ParseResult::Record(rec, _consumed) => {
                assert!(rec.raw.contains("error[E0308]"));
                assert!(rec.raw.contains("--> src/main.rs"));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn cargo_diag_ends_on_new_diagnostic() {
        let mut parser = CargoDiagnosticParser::new();
        let input = b"error[E0308]: mismatched types\n  --> src/main.rs:5:10\nwarning: unused variable\n";
        let result = parser.feed(input, false);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(rec.raw.contains("error[E0308]"));
                assert!(!rec.raw.contains("warning:"));
                // Should have consumed up to the start of the warning line
                assert!(consumed < input.len());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn cargo_diag_rejects_non_diagnostic() {
        let mut parser = CargoDiagnosticParser::new();
        let result = parser.feed(b"Compiling myapp v0.1.0\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    // -- LogfmtParser --

    #[test]
    fn logfmt_parses_basic() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed(b"level=info msg=\"hello world\" duration=1.23\n", false);
        match result {
            ParseResult::Record(rec, consumed) => {
                if let ParsedContent::Logfmt(pairs) = &rec.parsed {
                    assert_eq!(pairs.len(), 3);
                    assert_eq!(pairs[0], ("level".to_string(), "info".to_string()));
                    assert_eq!(pairs[1], ("msg".to_string(), "hello world".to_string()));
                    assert_eq!(pairs[2], ("duration".to_string(), "1.23".to_string()));
                } else {
                    panic!("expected Logfmt");
                }
                assert_eq!(consumed, 43); // includes trailing newline
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn logfmt_rejects_plain_text() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed(b"just plain text without equals\n", false);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn logfmt_empty_value() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed(b"key= other=val\n", false);
        match result {
            ParseResult::Record(rec, _consumed) => {
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

    #[test]
    fn logfmt_eof_without_newline() {
        let mut parser = LogfmtParser::new();
        let result = parser.feed(b"key=val", true);
        match result {
            ParseResult::Record(rec, consumed) => {
                if let ParsedContent::Logfmt(pairs) = &rec.parsed {
                    assert_eq!(pairs[0], ("key".to_string(), "val".to_string()));
                }
                assert_eq!(consumed, 7);
            }
            _ => panic!("expected Record at EOF"),
        }
    }

    // -- PlainLineParser --

    #[test]
    fn plain_line_with_newline() {
        let mut parser = PlainLineParser;
        let result = parser.feed(b"anything at all\n", false);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert_eq!(rec.raw, "anything at all");
                assert!(matches!(rec.parsed, ParsedContent::PlainText));
                assert_eq!(consumed, 16); // includes newline
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn plain_line_eof_no_newline() {
        let mut parser = PlainLineParser;
        let result = parser.feed(b"no trailing newline", true);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert_eq!(rec.raw, "no trailing newline");
                assert_eq!(consumed, 19);
            }
            _ => panic!("expected Record at EOF"),
        }
    }

    #[test]
    fn plain_line_incomplete_no_newline() {
        let mut parser = PlainLineParser;
        let result = parser.feed(b"partial data", false);
        assert!(matches!(result, ParseResult::Incomplete));
    }

    #[test]
    fn plain_line_empty_eof() {
        let mut parser = PlainLineParser;
        let result = parser.feed(b"", true);
        assert!(matches!(result, ParseResult::Rejection));
    }

    #[test]
    fn plain_line_empty_not_eof() {
        let mut parser = PlainLineParser;
        let result = parser.feed(b"", false);
        assert!(matches!(result, ParseResult::Incomplete));
    }

    // -- FallbackParser --

    #[test]
    fn fallback_json_wins_over_plain() {
        let mut parser = default_parser();
        let result = parser.feed(b"{\"level\":\"info\"}\n", false);
        match result {
            ParseResult::Record(rec, _consumed) => {
                assert!(matches!(rec.parsed, ParsedContent::Json(_)));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_plain_text_fallback() {
        let mut parser = default_parser();
        let result = parser.feed(b"just plain text\n", false);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert!(matches!(rec.parsed, ParsedContent::PlainText));
                assert_eq!(rec.raw, "just plain text");
                assert_eq!(consumed, 16);
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_logfmt_detected() {
        let mut parser = default_parser();
        let result = parser.feed(b"level=info msg=hello\n", false);
        match result {
            ParseResult::Record(rec, _consumed) => {
                assert!(matches!(rec.parsed, ParsedContent::Logfmt(_)));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_rust_panic_multiline() {
        let mut parser = default_parser();
        // First feed: just the panic line (no non-continuation line yet)
        let r1 = parser.feed(b"thread 'main' panicked at 'oh no', src/main.rs:1:1\nstack backtrace:\n", false);
        assert!(matches!(r1, ParseResult::Incomplete));

        // Now add a non-continuation line -- this should trigger the record
        let full_input = b"thread 'main' panicked at 'oh no', src/main.rs:1:1\nstack backtrace:\nsome other line\n";
        let r2 = parser.feed(full_input, false);
        match r2 {
            ParseResult::Record(rec, _consumed) => {
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
        let r1 = parser.feed(b"{\"msg\":\"hello\"}\n", false);
        assert!(matches!(r1, ParseResult::Record(RawRecord { parsed: ParsedContent::Json(_), .. }, _)));

        // Plain text fallback
        let r2 = parser.feed(b"plain line\n", false);
        assert!(matches!(r2, ParseResult::Record(RawRecord { parsed: ParsedContent::PlainText, .. }, _)));
    }

    // -- FallbackParser active parser tracking --

    #[test]
    fn fallback_active_parser_tracking() {
        let mut parser = default_parser();

        // Feed incomplete data that triggers Incomplete from RustPanicParser
        // (after JsonlParser rejects it)
        let r1 = parser.feed(b"thread 'main' panicked at 'oh no', src/main.rs:1:1\n", false);
        assert!(matches!(r1, ParseResult::Incomplete));

        // The active parser should be the RustPanicParser.
        // Now feed more data with the continuation + non-continuation
        let full = b"thread 'main' panicked at 'oh no', src/main.rs:1:1\nstack backtrace:\nnormal line\n";
        let r2 = parser.feed(full, false);
        match r2 {
            ParseResult::Record(rec, consumed) => {
                assert!(rec.raw.contains("thread 'main' panicked"));
                assert!(rec.raw.contains("stack backtrace:"));
                assert!(!rec.raw.contains("normal line"));
                assert!(consumed < full.len());
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn fallback_active_parser_reset() {
        let mut parser = default_parser();

        // Trigger Incomplete
        let _ = parser.feed(b"thread 'main' panicked at 'oh no', src/main.rs:1:1\n", false);

        // Reset should clear active
        parser.reset();

        // Now plain text should work
        let r = parser.feed(b"plain text\n", false);
        match r {
            ParseResult::Record(rec, _) => {
                assert!(matches!(rec.parsed, ParsedContent::PlainText));
            }
            _ => panic!("expected Record after reset"),
        }
    }

    // -- EOF behavior --

    #[test]
    fn eof_emits_remaining_data() {
        let mut parser = default_parser();
        // Data with no trailing newline at EOF
        let result = parser.feed(b"final line no newline", true);
        match result {
            ParseResult::Record(rec, consumed) => {
                assert_eq!(rec.raw, "final line no newline");
                assert_eq!(consumed, 21);
            }
            _ => panic!("expected Record at EOF"),
        }
    }
}
