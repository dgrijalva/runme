//! Filter expression engine for log entries.
//!
//! Supports Lucene/Datadog-style `key:value` filter syntax with boolean
//! operators (AND, OR, NOT), negation prefix (`-`), and various value
//! matchers (substring, exact, regex, comparison, wildcard).

mod eval;
mod parse;
mod types;

pub use eval::matches;
pub use parse::parse;
pub use types::*;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::execution::TaskId;
    use crate::log::{LogEntry, ParsedContent};

    fn tid(name: &str) -> TaskId {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        TaskId(h.finish())
    }

    /// Helper to create a simple LogEntry for testing.
    fn make_entry(raw: &str) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: tid("test"),
            seq: 0,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
            stream: None,
        }
    }

    /// Helper to create a LogEntry with well-known fields.
    fn make_rich_entry() -> LogEntry {
        let mut fields = HashMap::new();
        fields.insert(
            "service".to_string(),
            serde_json::Value::String("auth-api".to_string()),
        );
        fields.insert("status".to_string(), serde_json::json!(404));
        fields.insert(
            "http".to_string(),
            serde_json::json!({
                "method": "GET",
                "status": 500,
                "path": "/api/users"
            }),
        );

        LogEntry {
            received_at: chrono::Utc::now(),
            raw: "2024-01-01 ERROR connection refused to auth service".to_string(),
            parsed: ParsedContent::PlainText,
            source: tid("web-server"),
            seq: 42,
            timestamp: Some("2024-01-01T00:00:00Z".to_string()),
            level: Some("error".to_string()),
            message: Some("connection refused".to_string()),
            fields,
            stream: None,
        }
    }

    // -----------------------------------------------------------------------
    // Parser tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_simple_field_value() {
        let expr = parse("level:error").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(!term.negated);
                assert_eq!(term.field.as_ref().unwrap().0, vec!["level"]);
                assert!(std::matches!(&term.matcher, Matcher::Substring(s) if s == "error"));
            }
            _ => panic!("expected Term, got {:?}", expr),
        }
    }

    #[test]
    fn parse_dotted_field() {
        let expr = parse("http.status:500").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert_eq!(term.field.as_ref().unwrap().0, vec!["http", "status"]);
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_negated_term() {
        let expr = parse("-level:debug").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(term.negated);
                assert_eq!(term.field.as_ref().unwrap().0, vec!["level"]);
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_quoted_value() {
        let expr = parse("message:\"connection refused\"").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Exact(s) if s == "connection refused"
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_regex_value() {
        let expr = parse("message:/connect.*refused/").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(&term.matcher, Matcher::Regex(_)));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_comparison_gt() {
        let expr = parse("status:>400").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Comparison(CmpOp::Gt, v) if *v == 400.0
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_comparison_gte() {
        let expr = parse("status:>=400").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Comparison(CmpOp::Gte, v) if *v == 400.0
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_comparison_lt() {
        let expr = parse("status:<200").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Comparison(CmpOp::Lt, v) if *v == 200.0
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_comparison_lte() {
        let expr = parse("status:<=200").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Comparison(CmpOp::Lte, v) if *v == 200.0
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_wildcard_value() {
        let expr = parse("service:auth*").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Wildcard(s) if s == "auth*"
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_bare_text() {
        let expr = parse("connection").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(term.field.is_none());
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Substring(s) if s == "connection"
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn parse_implicit_and() {
        let expr = parse("level:error service:auth").unwrap();
        assert!(std::matches!(&expr, FilterExpr::And(_, _)));
    }

    #[test]
    fn parse_explicit_and() {
        let expr = parse("level:error AND service:auth").unwrap();
        assert!(std::matches!(&expr, FilterExpr::And(_, _)));
    }

    #[test]
    fn parse_or() {
        let expr = parse("level:error OR level:warn").unwrap();
        assert!(std::matches!(&expr, FilterExpr::Or(_, _)));
    }

    #[test]
    fn parse_not() {
        let expr = parse("NOT level:debug").unwrap();
        assert!(std::matches!(&expr, FilterExpr::Not(_)));
    }

    #[test]
    fn parse_parenthesized() {
        let expr = parse("level:error AND (service:auth OR service:api)").unwrap();
        match &expr {
            FilterExpr::And(left, right) => {
                assert!(std::matches!(left.as_ref(), FilterExpr::Term(_)));
                assert!(std::matches!(right.as_ref(), FilterExpr::Or(_, _)));
            }
            _ => panic!("expected And with Or inside"),
        }
    }

    #[test]
    fn parse_complex_expression() {
        let expr = parse("level:error AND (service:auth OR service:api) -status:<200").unwrap();
        // Should parse without error -- complex nested expression
        assert!(std::matches!(&expr, FilterExpr::And(_, _)));
    }

    #[test]
    fn parse_empty_input() {
        let result = parse("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn parse_unclosed_paren() {
        let result = parse("(level:error");
        assert!(result.is_err());
    }

    #[test]
    fn parse_unclosed_quote() {
        let result = parse("message:\"unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn parse_unclosed_regex() {
        let result = parse("message:/unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn parse_invalid_regex() {
        let result = parse("message:/[invalid/");
        assert!(result.is_err());
    }

    #[test]
    fn parse_comparison_missing_number() {
        let result = parse("status:>abc");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Evaluator tests
    // -----------------------------------------------------------------------

    #[test]
    fn eval_bare_text_matches_raw() {
        let entry = make_entry("ERROR: connection refused");
        let expr = parse("connection").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_bare_text_no_match() {
        let entry = make_entry("everything is fine");
        let expr = parse("error").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_bare_text_matches_message() {
        let mut entry = make_entry("raw line");
        entry.message = Some("connection refused".to_string());
        let expr = parse("refused").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_bare_text_case_insensitive() {
        let entry = make_entry("ERROR: something happened");
        let expr = parse("error").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_level_field() {
        let entry = make_rich_entry();
        let expr = parse("level:error").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_level_field_no_match() {
        let entry = make_rich_entry();
        let expr = parse("level:info").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_message_field() {
        let entry = make_rich_entry();
        let expr = parse("message:refused").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_source_field() {
        // `source` filters now match against the TaskId Display ("t<N>")
        // since the storage layer dropped string source names.
        let entry = make_rich_entry();
        let expr = parse(&format!("source:{}", entry.source)).unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_timestamp_field() {
        let entry = make_rich_entry();
        let expr = parse("timestamp:2024").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_extension_field() {
        let entry = make_rich_entry();
        let expr = parse("service:auth-api").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_numeric_comparison() {
        let entry = make_rich_entry();
        let expr = parse("status:>400").unwrap();
        assert!(matches(&expr, &entry)); // status is 404
    }

    #[test]
    fn eval_numeric_comparison_no_match() {
        let entry = make_rich_entry();
        let expr = parse("status:>500").unwrap();
        assert!(!matches(&expr, &entry)); // status is 404
    }

    #[test]
    fn eval_dotted_field_traversal() {
        let entry = make_rich_entry();
        let expr = parse("http.status:>400").unwrap();
        assert!(matches(&expr, &entry)); // http.status is 500
    }

    #[test]
    fn eval_dotted_field_string() {
        let entry = make_rich_entry();
        let expr = parse("http.method:GET").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_negation() {
        let entry = make_rich_entry();
        let expr = parse("-level:debug").unwrap();
        assert!(matches(&expr, &entry)); // level is error, not debug
    }

    #[test]
    fn eval_negation_excludes() {
        let entry = make_rich_entry();
        let expr = parse("-level:error").unwrap();
        assert!(!matches(&expr, &entry)); // level IS error, so negation excludes
    }

    #[test]
    fn eval_and() {
        let entry = make_rich_entry();
        let expr = parse("level:error service:auth-api").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_and_partial_fail() {
        let entry = make_rich_entry();
        let expr = parse("level:error service:billing").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_or() {
        let entry = make_rich_entry();
        let expr = parse("level:info OR level:error").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_or_neither() {
        let entry = make_rich_entry();
        let expr = parse("level:info OR level:debug").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_not() {
        let entry = make_rich_entry();
        let expr = parse("NOT level:debug").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_not_excludes() {
        let entry = make_rich_entry();
        let expr = parse("NOT level:error").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_parenthesized_or() {
        let entry = make_rich_entry();
        let expr = parse("level:error AND (service:auth-api OR service:billing)").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_wildcard() {
        let entry = make_rich_entry();
        let expr = parse("service:auth*").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_wildcard_no_match() {
        let entry = make_rich_entry();
        let expr = parse("service:billing*").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_regex() {
        let entry = make_rich_entry();
        let expr = parse("message:/connect.*refused/").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_regex_no_match() {
        let entry = make_rich_entry();
        let expr = parse("message:/timeout/").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_quoted_exact_substring() {
        let entry = make_rich_entry();
        let expr = parse("message:\"connection refused\"").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_quoted_exact_no_match() {
        let entry = make_rich_entry();
        let expr = parse("message:\"Connection Refused\"").unwrap();
        // Exact match is case-sensitive
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_raw_field() {
        let entry = make_rich_entry();
        let expr = parse("raw:ERROR").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_missing_field() {
        let entry = make_rich_entry();
        let expr = parse("nonexistent:value").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_missing_dotted_field() {
        let entry = make_rich_entry();
        let expr = parse("http.nonexistent:value").unwrap();
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_comparison_on_non_numeric() {
        let entry = make_rich_entry();
        let expr = parse("service:>100").unwrap();
        // service is "auth-api", not a number -- comparison should fail
        assert!(!matches(&expr, &entry));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn eval_entry_with_no_level() {
        let entry = make_entry("plain text");
        let expr = parse("level:error").unwrap();
        // level is None, so no match
        assert!(!matches(&expr, &entry));
    }

    #[test]
    fn eval_multiple_implicit_and() {
        let entry = make_rich_entry();
        let expr = parse("level:error service:auth-api status:>400").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_wildcard_question_mark() {
        let entry = make_rich_entry();
        // "auth-api" should match "auth-?pi"
        let expr = parse("service:auth-?pi").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn parse_at_field_name() {
        // Support @timestamp style field names
        let expr = parse("@timestamp:2024").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert_eq!(term.field.as_ref().unwrap().0, vec!["@timestamp"]);
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn eval_comparison_decimal() {
        let mut entry = make_entry("test");
        entry
            .fields
            .insert("duration".to_string(), serde_json::json!(1.5));
        let expr = parse("duration:>1.0").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_deeply_nested_field() {
        let mut entry = make_entry("test");
        entry.fields.insert(
            "error".to_string(),
            serde_json::json!({
                "details": {
                    "code": "E001"
                }
            }),
        );
        let expr = parse("error.details.code:E001").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn parse_negated_bare_text() {
        let expr = parse("-debug").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(term.negated);
                assert!(term.field.is_none());
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Substring(s) if s == "debug"
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn eval_negated_bare_text() {
        let entry = make_entry("INFO: all good");
        let expr = parse("-debug").unwrap();
        // "debug" not in raw, so negation means this matches
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_or_precedence() {
        // OR should have lower precedence than AND (implicit)
        // "a AND b OR c" should be "(a AND b) OR c"
        let entry = make_rich_entry();
        let expr = parse("level:error service:auth-api OR level:info").unwrap();
        // level is error AND service is auth-api => true OR false => true
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_or_precedence_false_branch() {
        let entry = make_rich_entry();
        // level:info is false, service:billing is false
        let expr = parse("level:info service:billing OR level:error").unwrap();
        // (level:info AND service:billing) => false, OR level:error => true
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_not_paren() {
        let entry = make_rich_entry();
        let expr = parse("NOT(level:debug)").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn parse_escaped_quote_in_string() {
        let expr = parse(r#"message:"hello \"world\"""#).unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert!(std::matches!(
                    &term.matcher,
                    Matcher::Exact(s) if s == "hello \"world\""
                ));
            }
            _ => panic!("expected Term"),
        }
    }

    #[test]
    fn eval_comparison_negative_number() {
        let mut entry = make_entry("test");
        entry
            .fields
            .insert("temp".to_string(), serde_json::json!(-5));
        let expr = parse("temp:<0").unwrap();
        assert!(matches(&expr, &entry));
    }

    #[test]
    fn eval_boolean_json_value() {
        let mut entry = make_entry("test");
        entry
            .fields
            .insert("active".to_string(), serde_json::json!(true));
        let expr = parse("active:true").unwrap();
        assert!(matches(&expr, &entry));
    }
}
