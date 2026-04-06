use super::super::{ParseResult, ParsedContent, RawRecord};
use super::{next_line, RecordParser};

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
        // Use next_line for line scanning
        match next_line(data, eof) {
            ParseResult::Record(rec, consumed) => {
                // next_line gave us a line; try to parse as logfmt
                match Self::parse_logfmt(&rec.raw) {
                    Some(pairs) => ParseResult::Record(
                        RawRecord {
                            raw: rec.raw,
                            parsed: ParsedContent::Logfmt(pairs),
                        },
                        consumed,
                    ),
                    None => ParseResult::Rejection,
                }
            }
            ParseResult::Incomplete => ParseResult::Incomplete,
            other => other,
        }
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
