use regex::Regex;

use super::super::{ParseResult, ParsedContent, RawRecord};
use super::{next_line, RecordParser};

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
                    // Check if this line starts a NEW diagnostic (end of current one)
                    if self.start_re.is_match(&rec.raw) {
                        break;
                    }
                    if self.continuation_re.is_match(&rec.raw) {
                        offset += consumed;
                    } else {
                        // Non-matching, non-start line: end of diagnostic.
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
        let input =
            b"error[E0308]: mismatched types\n  --> src/main.rs:5:10\nwarning: unused variable\n";
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
}
