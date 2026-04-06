mod cargo_diag;
mod json;
mod logfmt;
mod plain;
mod rust_panic;

pub use cargo_diag::CargoDiagnosticParser;
pub use json::JsonlParser;
pub use logfmt::LogfmtParser;
pub use plain::PlainLineParser;
pub use rust_panic::RustPanicParser;

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
// next_line — shared line-scanning logic
// ---------------------------------------------------------------------------

/// Scan for the next newline-terminated line from the front of `data`.
///
/// Returns:
/// - `Record(RawRecord { raw, PlainText }, consumed)` when a full line
///   (or remaining bytes at EOF) is found. `consumed` includes the newline.
/// - `Incomplete` when no newline is found and `eof` is false.
/// - `Rejection` when `data` is empty and `eof` is true.
pub fn next_line(data: &[u8], eof: bool) -> ParseResult {
    if data.is_empty() {
        return if eof {
            ParseResult::Rejection
        } else {
            ParseResult::Incomplete
        };
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
