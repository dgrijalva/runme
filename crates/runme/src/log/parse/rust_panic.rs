use regex::Regex;

use super::super::{ParseResult, ParsedContent, RawRecord};
use super::{next_line, RecordParser};

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
        let mut offset = 0;

        // Get first line
        match next_line(&data[offset..], eof) {
            ParseResult::Record(rec, consumed) => {
                if !self.start_re.is_match(&rec.raw) {
                    return ParseResult::Rejection;
                }
                offset += consumed;
            }
            other => return other, // Incomplete or Rejection
        }

        // Get continuation lines
        loop {
            match next_line(&data[offset..], eof) {
                ParseResult::Record(rec, consumed) => {
                    if self.continuation_re.is_match(&rec.raw) {
                        offset += consumed;
                    } else {
                        // Non-continuation line found; stop here.
                        break;
                    }
                }
                ParseResult::Incomplete => {
                    if eof {
                        break;
                    }
                    return ParseResult::Incomplete;
                }
                _ => break,
            }
        }

        // Emit the full multiline record
        let raw = String::from_utf8_lossy(&data[..offset])
            .trim_end_matches('\n')
            .to_string();
        ParseResult::Record(
            RawRecord {
                raw,
                parsed: ParsedContent::PlainText,
            },
            offset,
        )
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_panic_detects_start() {
        let mut parser = RustPanicParser::new();
        let result = parser.feed(
            b"thread 'main' panicked at 'index out of bounds', src/main.rs:10:5\n",
            false,
        );
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
}
