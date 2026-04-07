use std::collections::HashMap;

use super::{ExtractedFields, ParsedContent, RawRecord};

/// Stateless, per-record field extraction. Takes a parsed record and
/// populates well-known fields on the LogEntry.
pub trait FieldExtractor: Send + Sync {
    /// Extract well-known fields from a parsed record.
    /// Returns the fields it found. Missing fields are simply absent.
    fn extract(&self, record: &RawRecord) -> ExtractedFields;
}

/// Blanket impl so `Box<dyn FieldExtractor>` can be used as `&dyn FieldExtractor`.
impl FieldExtractor for Box<dyn FieldExtractor> {
    fn extract(&self, record: &RawRecord) -> ExtractedFields {
        (**self).extract(record)
    }
}

// ---------------------------------------------------------------------------
// LayeredExtractor
// ---------------------------------------------------------------------------

/// Runs all extractors and merges results. Unlike parsers (where one wins),
/// extractors accumulate. First writer wins for well-known fields; the
/// `fields` HashMap merges all.
pub struct LayeredExtractor {
    extractors: Vec<Box<dyn FieldExtractor>>,
}

impl LayeredExtractor {
    pub fn new(extractors: Vec<Box<dyn FieldExtractor>>) -> Self {
        Self { extractors }
    }
}

impl FieldExtractor for LayeredExtractor {
    fn extract(&self, record: &RawRecord) -> ExtractedFields {
        let mut merged = ExtractedFields {
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
        };

        for extractor in &self.extractors {
            let extracted = extractor.extract(record);

            // First writer wins for well-known fields.
            if merged.timestamp.is_none() {
                merged.timestamp = extracted.timestamp;
            }
            if merged.level.is_none() {
                merged.level = extracted.level;
            }
            if merged.message.is_none() {
                merged.message = extracted.message;
            }

            // HashMap merges all (first writer wins per key).
            for (key, value) in extracted.fields {
                merged.fields.entry(key).or_insert(value);
            }
        }

        merged
    }
}

// ---------------------------------------------------------------------------
// CommonJsonFieldExtractor
// ---------------------------------------------------------------------------

/// Maps common JSON/logfmt field names to well-known fields using the
/// priority-ordered mapping table from the design document. Works with both
/// `ParsedContent::Json` and `ParsedContent::Logfmt`.
pub struct CommonJsonFieldExtractor;

impl CommonJsonFieldExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Level field candidates (priority order).
    const LEVEL_FIELDS: &'static [&'static str] = &[
        "level",
        "severity",
        "levelname",
        "lvl",
        "log_level",
        "loglevel",
        "log.level",
        "levelno",
    ];

    /// Message field candidates (priority order).
    const MESSAGE_FIELDS: &'static [&'static str] = &["msg", "message", "event", "text", "body"];

    /// Timestamp field candidates (priority order).
    const TIMESTAMP_FIELDS: &'static [&'static str] = &[
        "timestamp",
        "time",
        "ts",
        "@timestamp",
        "datetime",
        "asctime",
        "created",
        "timeMillis",
    ];

    /// Additional well-known field groups: (semantic name, candidate field names).
    const ADDITIONAL_FIELDS: &'static [(&'static str, &'static [&'static str])] = &[
        (
            "caller",
            &["caller", "source", "logger", "logger_name", "name"],
        ),
        ("error", &["error", "err", "exception", "error.message"]),
        (
            "stack_trace",
            &[
                "stack_trace",
                "stacktrace",
                "stack",
                "error.stack_trace",
                "exception.stacktrace",
            ],
        ),
        ("hostname", &["hostname", "host", "host.name"]),
        ("pid", &["pid", "process", "process.pid"]),
        (
            "service",
            &["service", "service.name", "app", "application"],
        ),
        (
            "trace_id",
            &["trace_id", "traceId", "trace.id", "dd.trace_id"],
        ),
        ("span_id", &["span_id", "spanId", "span.id", "dd.span_id"]),
        (
            "request_id",
            &["request_id", "requestId", "req_id", "x-request-id"],
        ),
    ];

    /// Look up a field by name in a JSON value. Handles dotted keys by
    /// traversing into nested objects.
    fn json_get<'a>(val: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
        if let Some(v) = val.get(field) {
            return Some(v);
        }
        // Try dotted traversal
        if field.contains('.') {
            let parts: Vec<&str> = field.split('.').collect();
            let mut current = val;
            for part in &parts {
                match current.get(part) {
                    Some(v) => current = v,
                    None => return None,
                }
            }
            return Some(current);
        }
        None
    }

    /// Convert a JSON value to a string for well-known fields.
    fn value_to_string(val: &serde_json::Value) -> String {
        match val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => val.to_string(),
        }
    }

    /// Convert Python levelno to level name.
    fn levelno_to_name(val: &serde_json::Value) -> Option<String> {
        let n = val.as_u64()?;
        Some(
            match n {
                0..=10 => "DEBUG",
                11..=20 => "INFO",
                21..=30 => "WARNING",
                31..=40 => "ERROR",
                41.. => "CRITICAL",
            }
            .to_string(),
        )
    }

    /// Look up a field in logfmt key-value pairs.
    fn logfmt_get<'a>(pairs: &'a [(String, String)], field: &str) -> Option<&'a str> {
        pairs
            .iter()
            .find(|(k, _)| k == field)
            .map(|(_, v)| v.as_str())
    }

    fn extract_from_json(&self, val: &serde_json::Value) -> ExtractedFields {
        let mut fields = HashMap::new();

        // Extract level
        let mut level = None;
        for &field_name in Self::LEVEL_FIELDS {
            if let Some(v) = Self::json_get(val, field_name) {
                if field_name == "levelno" {
                    level = Self::levelno_to_name(v);
                } else {
                    level = Some(Self::value_to_string(v));
                }
                break;
            }
        }

        // Extract message
        let mut message = None;
        for &field_name in Self::MESSAGE_FIELDS {
            if let Some(v) = Self::json_get(val, field_name) {
                message = Some(Self::value_to_string(v));
                break;
            }
        }

        // Extract timestamp
        let mut timestamp = None;
        for &field_name in Self::TIMESTAMP_FIELDS {
            if let Some(v) = Self::json_get(val, field_name) {
                timestamp = Some(Self::value_to_string(v));
                break;
            }
        }

        // Extract additional well-known fields
        for &(semantic_name, candidates) in Self::ADDITIONAL_FIELDS {
            for &candidate in candidates {
                if let Some(v) = Self::json_get(val, candidate) {
                    fields
                        .entry(semantic_name.to_string())
                        .or_insert_with(|| v.clone());
                    break;
                }
            }
        }

        ExtractedFields {
            timestamp,
            level,
            message,
            fields,
        }
    }

    fn extract_from_logfmt(&self, pairs: &[(String, String)]) -> ExtractedFields {
        let mut fields = HashMap::new();

        // Extract level
        let mut level = None;
        for &field_name in Self::LEVEL_FIELDS {
            if let Some(v) = Self::logfmt_get(pairs, field_name) {
                if field_name == "levelno" {
                    if let Ok(n) = v.parse::<u64>() {
                        level = Some(
                            match n {
                                0..=10 => "DEBUG",
                                11..=20 => "INFO",
                                21..=30 => "WARNING",
                                31..=40 => "ERROR",
                                41.. => "CRITICAL",
                            }
                            .to_string(),
                        );
                    } else {
                        level = Some(v.to_string());
                    }
                } else {
                    level = Some(v.to_string());
                }
                break;
            }
        }

        // Extract message
        let mut message = None;
        for &field_name in Self::MESSAGE_FIELDS {
            if let Some(v) = Self::logfmt_get(pairs, field_name) {
                message = Some(v.to_string());
                break;
            }
        }

        // Extract timestamp
        let mut timestamp = None;
        for &field_name in Self::TIMESTAMP_FIELDS {
            if let Some(v) = Self::logfmt_get(pairs, field_name) {
                timestamp = Some(v.to_string());
                break;
            }
        }

        // Extract additional well-known fields
        for &(semantic_name, candidates) in Self::ADDITIONAL_FIELDS {
            for &candidate in candidates {
                if let Some(v) = Self::logfmt_get(pairs, candidate) {
                    fields
                        .entry(semantic_name.to_string())
                        .or_insert_with(|| serde_json::Value::String(v.to_string()));
                    break;
                }
            }
        }

        ExtractedFields {
            timestamp,
            level,
            message,
            fields,
        }
    }
}

impl Default for CommonJsonFieldExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldExtractor for CommonJsonFieldExtractor {
    fn extract(&self, record: &RawRecord) -> ExtractedFields {
        match &record.parsed {
            ParsedContent::Json(val) => self.extract_from_json(val),
            ParsedContent::Logfmt(pairs) => self.extract_from_logfmt(pairs),
            ParsedContent::PlainText => ExtractedFields {
                timestamp: None,
                level: None,
                message: None,
                fields: HashMap::new(),
            },
        }
    }
}

/// Build the default extractor.
pub fn default_extractor() -> LayeredExtractor {
    LayeredExtractor::new(vec![Box::new(CommonJsonFieldExtractor::new())])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- CommonJsonFieldExtractor with JSON --

    #[test]
    fn json_extract_level_msg_timestamp() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: r#"{"level":"info","msg":"hello","timestamp":"2024-01-01T00:00:00Z"}"#.to_string(),
            parsed: ParsedContent::Json(serde_json::json!({
                "level": "info",
                "msg": "hello",
                "timestamp": "2024-01-01T00:00:00Z"
            })),
        };
        let fields = extractor.extract(&record);
        assert_eq!(fields.level.as_deref(), Some("info"));
        assert_eq!(fields.message.as_deref(), Some("hello"));
        assert_eq!(fields.timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn json_extract_severity() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::Json(serde_json::json!({
                "severity": "WARNING",
                "message": "watch out"
            })),
        };
        let fields = extractor.extract(&record);
        assert_eq!(fields.level.as_deref(), Some("WARNING"));
        assert_eq!(fields.message.as_deref(), Some("watch out"));
    }

    #[test]
    fn json_extract_python_levelno() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::Json(serde_json::json!({
                "levelno": 30,
                "message": "a warning"
            })),
        };
        let fields = extractor.extract(&record);
        assert_eq!(fields.level.as_deref(), Some("WARNING"));
    }

    #[test]
    fn json_extract_zap_style() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::Json(serde_json::json!({
                "level": "error",
                "msg": "connection refused",
                "ts": 1704067200.123,
                "caller": "server.go:42"
            })),
        };
        let fields = extractor.extract(&record);
        assert_eq!(fields.level.as_deref(), Some("error"));
        assert_eq!(fields.message.as_deref(), Some("connection refused"));
        assert_eq!(fields.timestamp.as_deref(), Some("1704067200.123"));
        assert_eq!(
            fields.fields.get("caller"),
            Some(&serde_json::Value::String("server.go:42".to_string()))
        );
    }

    #[test]
    fn json_extract_elasticsearch_style() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::Json(serde_json::json!({
                "log.level": "info",
                "@timestamp": "2024-01-01T00:00:00Z",
                "message": "request handled",
                "service.name": "auth-api",
                "trace.id": "abc123"
            })),
        };
        let fields = extractor.extract(&record);
        // log.level is priority 7 for level, but "level" (priority 1) is not present
        // Actually "log.level" needs dotted key traversal. Let's check...
        // In the JSON, "log.level" is a flat key, not nested. json_get first tries
        // val.get("log.level") which should work for flat keys.
        assert_eq!(fields.level.as_deref(), Some("info"));
        assert_eq!(fields.timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(fields.message.as_deref(), Some("request handled"));
    }

    #[test]
    fn json_extract_additional_fields() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::Json(serde_json::json!({
                "level": "info",
                "msg": "req",
                "hostname": "web-01",
                "pid": 1234,
                "trace_id": "abc",
                "span_id": "def",
                "request_id": "req-123"
            })),
        };
        let fields = extractor.extract(&record);
        assert_eq!(
            fields.fields.get("hostname"),
            Some(&serde_json::Value::String("web-01".to_string()))
        );
        assert_eq!(fields.fields.get("pid"), Some(&serde_json::json!(1234)));
        assert_eq!(
            fields.fields.get("trace_id"),
            Some(&serde_json::Value::String("abc".to_string()))
        );
    }

    #[test]
    fn json_plain_text_returns_nothing() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: "plain text".to_string(),
            parsed: ParsedContent::PlainText,
        };
        let fields = extractor.extract(&record);
        assert!(fields.level.is_none());
        assert!(fields.message.is_none());
        assert!(fields.timestamp.is_none());
        assert!(fields.fields.is_empty());
    }

    // -- CommonJsonFieldExtractor with Logfmt --

    #[test]
    fn logfmt_extract_basic() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: "level=info msg=hello timestamp=2024-01-01T00:00:00Z".to_string(),
            parsed: ParsedContent::Logfmt(vec![
                ("level".to_string(), "info".to_string()),
                ("msg".to_string(), "hello".to_string()),
                ("timestamp".to_string(), "2024-01-01T00:00:00Z".to_string()),
            ]),
        };
        let fields = extractor.extract(&record);
        assert_eq!(fields.level.as_deref(), Some("info"));
        assert_eq!(fields.message.as_deref(), Some("hello"));
        assert_eq!(fields.timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn logfmt_extract_additional_fields() {
        let extractor = CommonJsonFieldExtractor::new();
        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::Logfmt(vec![
                ("level".to_string(), "error".to_string()),
                ("msg".to_string(), "fail".to_string()),
                ("caller".to_string(), "main.go:10".to_string()),
                ("service".to_string(), "api".to_string()),
            ]),
        };
        let fields = extractor.extract(&record);
        assert_eq!(
            fields.fields.get("caller"),
            Some(&serde_json::Value::String("main.go:10".to_string()))
        );
        assert_eq!(
            fields.fields.get("service"),
            Some(&serde_json::Value::String("api".to_string()))
        );
    }

    // -- LayeredExtractor --

    #[test]
    fn layered_first_writer_wins() {
        // Create two extractors that both set level
        struct ExtractorA;
        impl FieldExtractor for ExtractorA {
            fn extract(&self, _record: &RawRecord) -> ExtractedFields {
                ExtractedFields {
                    level: Some("from_a".to_string()),
                    message: Some("msg_a".to_string()),
                    timestamp: None,
                    fields: HashMap::new(),
                }
            }
        }

        struct ExtractorB;
        impl FieldExtractor for ExtractorB {
            fn extract(&self, _record: &RawRecord) -> ExtractedFields {
                ExtractedFields {
                    level: Some("from_b".to_string()),
                    message: Some("msg_b".to_string()),
                    timestamp: Some("ts_b".to_string()),
                    fields: HashMap::new(),
                }
            }
        }

        let layered = LayeredExtractor::new(vec![Box::new(ExtractorA), Box::new(ExtractorB)]);

        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::PlainText,
        };

        let fields = layered.extract(&record);
        // First writer wins for level and message
        assert_eq!(fields.level.as_deref(), Some("from_a"));
        assert_eq!(fields.message.as_deref(), Some("msg_a"));
        // Timestamp only set by B
        assert_eq!(fields.timestamp.as_deref(), Some("ts_b"));
    }

    #[test]
    fn layered_hashmap_merges_all() {
        struct ExtractorA;
        impl FieldExtractor for ExtractorA {
            fn extract(&self, _record: &RawRecord) -> ExtractedFields {
                let mut fields = HashMap::new();
                fields.insert("key_a".to_string(), serde_json::json!("val_a"));
                fields.insert("shared".to_string(), serde_json::json!("from_a"));
                ExtractedFields {
                    level: None,
                    message: None,
                    timestamp: None,
                    fields,
                }
            }
        }

        struct ExtractorB;
        impl FieldExtractor for ExtractorB {
            fn extract(&self, _record: &RawRecord) -> ExtractedFields {
                let mut fields = HashMap::new();
                fields.insert("key_b".to_string(), serde_json::json!("val_b"));
                fields.insert("shared".to_string(), serde_json::json!("from_b"));
                ExtractedFields {
                    level: None,
                    message: None,
                    timestamp: None,
                    fields,
                }
            }
        }

        let layered = LayeredExtractor::new(vec![Box::new(ExtractorA), Box::new(ExtractorB)]);

        let record = RawRecord {
            raw: String::new(),
            parsed: ParsedContent::PlainText,
        };

        let fields = layered.extract(&record);
        // Both keys present
        assert_eq!(
            fields.fields.get("key_a"),
            Some(&serde_json::json!("val_a"))
        );
        assert_eq!(
            fields.fields.get("key_b"),
            Some(&serde_json::json!("val_b"))
        );
        // Shared key: first writer wins
        assert_eq!(
            fields.fields.get("shared"),
            Some(&serde_json::json!("from_a"))
        );
    }

    // -- LogEntry construction --

    #[test]
    fn log_entry_construction() {
        use super::super::LogEntry;

        let entry = LogEntry {
            received_at: chrono::Utc::now(),
            raw: "test line".to_string(),
            parsed: ParsedContent::PlainText,
            source: "test_task".to_string(),
            seq: 0,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some("test".to_string()),
            fields: HashMap::new(),
        };

        assert_eq!(entry.raw, "test line");
        assert_eq!(entry.source, "test_task");
        assert_eq!(entry.seq, 0);
        assert_eq!(entry.level.as_deref(), Some("info"));
        assert_eq!(entry.as_str(), "test line");

        // Clone works (required by broadcast)
        let cloned = entry.clone();
        assert_eq!(cloned.raw, entry.raw);
    }
}
