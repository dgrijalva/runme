use super::super::{ParseResult, ParsedContent, RawRecord};
use super::RecordParser;

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
        Self {
            json_record_count: 0,
        }
    }

    /// Skip leading whitespace/newlines in the byte slice.
    /// Returns the number of bytes skipped.
    fn skip_whitespace(data: &[u8]) -> usize {
        let mut i = 0;
        while i < data.len()
            && (data[i] == b' ' || data[i] == b'\t' || data[i] == b'\n' || data[i] == b'\r')
        {
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
        while i < data.len()
            && (data[i] == b' ' || data[i] == b'\t' || data[i] == b'\n' || data[i] == b'\r')
        {
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
            return if eof {
                ParseResult::Rejection
            } else {
                ParseResult::Incomplete
            };
        }

        let ws_skip = Self::skip_whitespace(data);
        if ws_skip >= data.len() {
            // All whitespace
            return if eof {
                ParseResult::Rejection
            } else {
                ParseResult::Incomplete
            };
        }

        let first_byte = data[ws_skip];

        // Check if it starts with { or [
        if first_byte != b'{' && first_byte != b'[' {
            // Not JSON. If we've seen 3+ JSON records, reject (anomaly detection --
            // the next parser handles it).
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
