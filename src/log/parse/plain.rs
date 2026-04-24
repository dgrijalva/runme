use super::super::ParseResult;
use super::{RecordParser, next_line};

/// Scans for `\n`. Returns everything up to the newline as one record.
/// At EOF, emits whatever remains. Always succeeds -- terminal fallback.
pub struct PlainLineParser;

impl RecordParser for PlainLineParser {
    fn feed(&mut self, data: &[u8], eof: bool) -> ParseResult {
        next_line(data, eof)
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::ParsedContent;

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
}
