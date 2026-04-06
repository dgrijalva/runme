//! Filter expression engine for log entries.
//!
//! Supports Lucene/Datadog-style `key:value` filter syntax with boolean
//! operators (AND, OR, NOT), negation prefix (`-`), and various value
//! matchers (substring, exact, regex, comparison, wildcard).

use std::fmt;

use regex::Regex;

use super::LogEntry;

// ---------------------------------------------------------------------------
// AST Types
// ---------------------------------------------------------------------------

/// A filter expression AST node.
#[derive(Debug)]
pub enum FilterExpr {
    And(Box<FilterExpr>, Box<FilterExpr>),
    Or(Box<FilterExpr>, Box<FilterExpr>),
    Not(Box<FilterExpr>),
    Term(FilterTerm),
}

/// A single filter term: an optional field path and a matcher.
#[derive(Debug)]
pub struct FilterTerm {
    pub negated: bool,
    pub field: Option<FieldPath>,
    pub matcher: Matcher,
}

/// A dotted field path, e.g. `error.message` -> `["error", "message"]`.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldPath(pub Vec<String>);

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.join("."))
    }
}

/// How to match a field value.
#[derive(Debug)]
pub enum Matcher {
    /// Exact substring match (from quoted strings).
    Exact(String),
    /// Substring match (from bare words, case-insensitive).
    Substring(String),
    /// Regex match.
    Regex(Regex),
    /// Numeric comparison.
    Comparison(CmpOp, f64),
    /// Wildcard match (`*` and `?`). Stored as the original pattern;
    /// compiled to regex internally for evaluation.
    Wildcard(String),
}

/// Comparison operators for numeric matching.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a filter expression string into an AST.
///
/// Returns an error with a descriptive message if the input is malformed.
pub fn parse(input: &str) -> Result<FilterExpr, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty filter expression".to_string());
    }
    let mut stream = input;
    match parse_query(&mut stream) {
        Ok(expr) => {
            let remaining = stream.trim();
            if !remaining.is_empty() {
                Err(format!("unexpected trailing input: {:?}", remaining))
            } else {
                Ok(expr)
            }
        }
        Err(msg) => Err(format!("parse error: {}", msg)),
    }
}

/// Evaluate a filter expression against a log entry.
///
/// The filter is a view -- it never modifies the LogEntry.
pub fn matches(expr: &FilterExpr, entry: &LogEntry) -> bool {
    match expr {
        FilterExpr::And(left, right) => matches(left, entry) && matches(right, entry),
        FilterExpr::Or(left, right) => matches(left, entry) || matches(right, entry),
        FilterExpr::Not(inner) => !matches(inner, entry),
        FilterExpr::Term(term) => {
            let result = match_term(term, entry);
            if term.negated { !result } else { result }
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluator internals
// ---------------------------------------------------------------------------

/// Match a single term against a log entry.
fn match_term(term: &FilterTerm, entry: &LogEntry) -> bool {
    match &term.field {
        Some(field) => match_field(field, &term.matcher, entry),
        None => match_bare_text(&term.matcher, entry),
    }
}

/// Match a bare text term (no field prefix) -- searches across `raw` and `message`.
fn match_bare_text(matcher: &Matcher, entry: &LogEntry) -> bool {
    let mut targets: Vec<&str> = vec![entry.raw.as_str()];
    if let Some(ref msg) = entry.message {
        targets.push(msg.as_str());
    }

    for target in &targets {
        if match_value(matcher, target) {
            return true;
        }
    }
    false
}

/// Resolve a field path and match against the resolved value.
fn match_field(field: &FieldPath, matcher: &Matcher, entry: &LogEntry) -> bool {
    let values = resolve_field(field, entry);
    for val in &values {
        if match_value(matcher, val) {
            return true;
        }
    }
    false
}

/// Resolve a field path to string values from the log entry.
///
/// Well-known fields are checked first, then the fields HashMap.
/// Dotted keys traverse into nested serde_json::Value.
fn resolve_field(field: &FieldPath, entry: &LogEntry) -> Vec<String> {
    let field_name = field.0.join(".");

    // Check well-known fields first.
    match field_name.as_str() {
        "level" => return entry.level.iter().cloned().collect(),
        "message" => return entry.message.iter().cloned().collect(),
        "timestamp" => return entry.timestamp.iter().cloned().collect(),
        "source" => return vec![entry.source.clone()],
        "raw" => return vec![entry.raw.clone()],
        _ => {}
    }

    // Look up in the fields HashMap.
    if field.0.len() == 1 {
        // Simple key lookup.
        if let Some(val) = entry.fields.get(&field.0[0]) {
            return vec![json_value_to_string(val)];
        }
    } else {
        // Dotted key: first try the full dotted key as a flat HashMap key.
        if let Some(val) = entry.fields.get(&field_name) {
            return vec![json_value_to_string(val)];
        }
        // Then try traversing into nested serde_json::Value.
        // The first segment is the HashMap key, remaining segments traverse.
        if let Some(root_val) = entry.fields.get(&field.0[0]) {
            let mut current = root_val;
            for segment in &field.0[1..] {
                match current.get(segment.as_str()) {
                    Some(v) => current = v,
                    None => return vec![],
                }
            }
            return vec![json_value_to_string(current)];
        }
    }

    vec![]
}

/// Convert a serde_json::Value to a string for matching.
fn json_value_to_string(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Match a value string against a Matcher.
fn match_value(matcher: &Matcher, value: &str) -> bool {
    match matcher {
        Matcher::Exact(pattern) => value.contains(pattern.as_str()),
        Matcher::Substring(pattern) => {
            let lower_val = value.to_lowercase();
            let lower_pat = pattern.to_lowercase();
            lower_val.contains(&lower_pat)
        }
        Matcher::Regex(re) => re.is_match(value),
        Matcher::Comparison(op, threshold) => {
            if let Ok(num) = value.parse::<f64>() {
                match op {
                    CmpOp::Gt => num > *threshold,
                    CmpOp::Gte => num >= *threshold,
                    CmpOp::Lt => num < *threshold,
                    CmpOp::Lte => num <= *threshold,
                }
            } else {
                false
            }
        }
        Matcher::Wildcard(pattern) => {
            let regex_pattern = wildcard_to_regex(pattern);
            if let Ok(re) = Regex::new(&regex_pattern) {
                re.is_match(value)
            } else {
                false
            }
        }
    }
}

/// Convert a wildcard pattern (with `*` and `?`) to a regex string.
fn wildcard_to_regex(pattern: &str) -> String {
    let mut regex = String::from("(?i)^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            c if regex_syntax_needs_escape(c) => {
                regex.push('\\');
                regex.push(c);
            }
            c => regex.push(c),
        }
    }
    regex.push('$');
    regex
}

/// Characters that need escaping in regex.
fn regex_syntax_needs_escape(c: char) -> bool {
    std::matches!(
        c,
        '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
    )
}

// ---------------------------------------------------------------------------
// Parser (hand-rolled recursive descent, using winnow error types for
// descriptive error messages)
// ---------------------------------------------------------------------------

/// Parse a query: `or_expr`
fn parse_query(input: &mut &str) -> Result<FilterExpr, String> {
    parse_or_expr(input)
}

/// Handle OR precedence: `and_expr (OR and_expr)*`
fn parse_or_expr(input: &mut &str) -> Result<FilterExpr, String> {
    let mut left = parse_and_expr(input)?;
    loop {
        skip_spaces(input);
        if input.starts_with("OR ") || input.starts_with("OR)") {
            // Consume "OR"
            *input = &input[2..];
            skip_spaces(input);
            let right = parse_and_expr(input)?;
            left = FilterExpr::Or(Box::new(left), Box::new(right));
        } else if *input == "OR" {
            // "OR" at end of input with nothing after
            return Err("expected expression after OR".to_string());
        } else {
            break;
        }
    }
    Ok(left)
}

/// Handle AND precedence and implicit AND: `unary_expr ((AND)? unary_expr)*`
fn parse_and_expr(input: &mut &str) -> Result<FilterExpr, String> {
    let mut left = parse_unary_expr(input)?;
    loop {
        skip_spaces(input);
        if input.is_empty() || input.starts_with(')') {
            break;
        }
        // Check for OR -- if we see OR, let the caller handle it
        if input.starts_with("OR ") || input.starts_with("OR)") || *input == "OR" {
            break;
        }
        // Explicit AND
        if input.starts_with("AND ") || input.starts_with("AND)") {
            *input = &input[3..];
            skip_spaces(input);
        } else if *input == "AND" {
            return Err("expected expression after AND".to_string());
        }
        // Check again after consuming AND
        if input.is_empty() || input.starts_with(')') {
            break;
        }
        if input.starts_with("OR ") || input.starts_with("OR)") || *input == "OR" {
            break;
        }
        let right = parse_unary_expr(input)?;
        left = FilterExpr::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

/// Handle NOT prefix and parenthesized groups: `NOT expr | '(' query ')' | term`
fn parse_unary_expr(input: &mut &str) -> Result<FilterExpr, String> {
    skip_spaces(input);
    if input.starts_with("NOT ") || input.starts_with("NOT(") {
        *input = &input[3..];
        skip_spaces(input);
        let inner = parse_unary_expr(input)?;
        return Ok(FilterExpr::Not(Box::new(inner)));
    }
    if input.starts_with('(') {
        *input = &input[1..];
        skip_spaces(input);
        let inner = parse_query(input)?;
        skip_spaces(input);
        if input.starts_with(')') {
            *input = &input[1..];
        } else {
            return Err("expected closing parenthesis ')'".to_string());
        }
        return Ok(inner);
    }
    parse_term_expr(input)
}

/// Parse a single term: `'-'? field ':' value | '-'? bare_text`
fn parse_term_expr(input: &mut &str) -> Result<FilterExpr, String> {
    skip_spaces(input);

    if input.is_empty() {
        return Err("expected filter term".to_string());
    }

    // Check for negation prefix.
    let negated = if input.starts_with('-') {
        *input = &input[1..];
        true
    } else {
        false
    };

    // Try to parse as field:value first.
    if let Some(colon_pos) = find_field_colon(input) {
        let field_str = &input[..colon_pos];
        if !field_str.is_empty() && is_valid_field(field_str) {
            let field = parse_field_path(field_str);
            *input = &input[colon_pos + 1..];
            let matcher = parse_value(input)?;
            return Ok(FilterExpr::Term(FilterTerm {
                negated,
                field: Some(field),
                matcher,
            }));
        }
    }

    // Fall through to bare text.
    let text = parse_bare_word(input);
    if text.is_empty() {
        return Err("expected filter term".to_string());
    }

    let matcher = if contains_wildcard(&text) {
        Matcher::Wildcard(text)
    } else {
        Matcher::Substring(text)
    };

    Ok(FilterExpr::Term(FilterTerm {
        negated,
        field: None,
        matcher,
    }))
}

/// Find the position of the colon that separates field from value.
fn find_field_colon(input: &str) -> Option<usize> {
    for (i, ch) in input.char_indices() {
        if ch == ':' {
            return Some(i);
        }
        if ch == ' ' || ch == '(' || ch == ')' {
            return None;
        }
        if !is_field_char(ch) {
            return None;
        }
    }
    None
}

/// Check if a character is valid in a field name.
fn is_field_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '-' || c == '@'
}

/// Check if a string is a valid field name (not a keyword).
fn is_valid_field(s: &str) -> bool {
    if s == "AND" || s == "OR" || s == "NOT" {
        return false;
    }
    s.chars().all(is_field_char)
}

/// Parse a dotted field path string into a FieldPath.
fn parse_field_path(s: &str) -> FieldPath {
    FieldPath(s.split('.').map(|seg| seg.to_string()).collect())
}

/// Parse a value after the colon in a field:value term.
fn parse_value(input: &mut &str) -> Result<Matcher, String> {
    if input.is_empty() {
        return Err("expected value after ':'".to_string());
    }

    // Comparison value: >=, >, <=, <
    if input.starts_with(">=")
        || input.starts_with('>')
        || input.starts_with("<=")
        || input.starts_with('<')
    {
        return parse_comparison(input);
    }

    // Regex value: /pattern/
    if input.starts_with('/') {
        return parse_regex(input);
    }

    // Quoted string: "..."
    if input.starts_with('"') {
        return parse_quoted_string(input);
    }

    // Bare word (may contain wildcards).
    let word = parse_bare_word(input);
    if word.is_empty() {
        return Err("expected value".to_string());
    }

    if contains_wildcard(&word) {
        Ok(Matcher::Wildcard(word))
    } else {
        Ok(Matcher::Substring(word))
    }
}

/// Parse a comparison value: `>N`, `>=N`, `<N`, `<=N`.
fn parse_comparison(input: &mut &str) -> Result<Matcher, String> {
    let (op, skip) = if input.starts_with(">=") {
        (CmpOp::Gte, 2)
    } else if input.starts_with('>') {
        (CmpOp::Gt, 1)
    } else if input.starts_with("<=") {
        (CmpOp::Lte, 2)
    } else {
        (CmpOp::Lt, 1)
    };
    *input = &input[skip..];

    let num_str = parse_number_str(input);
    if num_str.is_empty() {
        return Err("expected number after comparison operator".to_string());
    }
    let num: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid number: {:?}", num_str))?;
    Ok(Matcher::Comparison(op, num))
}

/// Parse a number string (digits, optional decimal point, optional leading minus).
fn parse_number_str(input: &mut &str) -> String {
    let mut result = String::new();
    let mut seen_dot = false;

    // Optional leading minus for negative numbers.
    if input.starts_with('-') {
        result.push('-');
        *input = &input[1..];
    }

    while let Some(ch) = input.chars().next() {
        if ch.is_ascii_digit() {
            result.push(ch);
            *input = &input[ch.len_utf8()..];
        } else if ch == '.' && !seen_dot {
            seen_dot = true;
            result.push(ch);
            *input = &input[1..];
        } else {
            break;
        }
    }
    result
}

/// Parse a regex value: `/pattern/`.
fn parse_regex(input: &mut &str) -> Result<Matcher, String> {
    // Consume opening '/'.
    *input = &input[1..];
    let mut pattern = String::new();
    let mut escaped = false;

    loop {
        if input.is_empty() {
            return Err("expected closing '/' for regex".to_string());
        }
        let ch = input.chars().next().unwrap();
        *input = &input[ch.len_utf8()..];

        if escaped {
            pattern.push(ch);
            escaped = false;
        } else if ch == '\\' {
            pattern.push(ch);
            escaped = true;
        } else if ch == '/' {
            break;
        } else {
            pattern.push(ch);
        }
    }

    Regex::new(&pattern).map(Matcher::Regex).map_err(|e| {
        format!("invalid regex /{}/: {}", pattern, e)
    })
}

/// Parse a quoted string: `"..."`.
fn parse_quoted_string(input: &mut &str) -> Result<Matcher, String> {
    // Consume opening '"'.
    *input = &input[1..];
    let mut value = String::new();
    let mut escaped = false;

    loop {
        if input.is_empty() {
            return Err("expected closing '\"' for quoted string".to_string());
        }
        let ch = input.chars().next().unwrap();
        *input = &input[ch.len_utf8()..];

        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            break;
        } else {
            value.push(ch);
        }
    }
    Ok(Matcher::Exact(value))
}

/// Parse a bare word (no spaces, no parens).
fn parse_bare_word(input: &mut &str) -> String {
    let mut word = String::new();
    while let Some(ch) = input.chars().next() {
        if ch == ' ' || ch == ')' || ch == '(' {
            break;
        }
        word.push(ch);
        *input = &input[ch.len_utf8()..];
    }
    word
}

/// Check if a string contains wildcard characters.
fn contains_wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Skip whitespace.
fn skip_spaces(input: &mut &str) {
    *input = input.trim_start();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::log::{LogEntry, ParsedContent};

    /// Helper to create a simple LogEntry for testing.
    fn make_entry(raw: &str) -> LogEntry {
        LogEntry {
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: "test".to_string(),
            seq: 0,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
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
            raw: "2024-01-01 ERROR connection refused to auth service".to_string(),
            parsed: ParsedContent::PlainText,
            source: "web-server".to_string(),
            seq: 42,
            timestamp: Some("2024-01-01T00:00:00Z".to_string()),
            level: Some("error".to_string()),
            message: Some("connection refused".to_string()),
            fields,
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
                assert!(
                    std::matches!(&term.matcher, Matcher::Substring(s) if s == "error")
                );
            }
            _ => panic!("expected Term, got {:?}", expr),
        }
    }

    #[test]
    fn parse_dotted_field() {
        let expr = parse("http.status:500").unwrap();
        match &expr {
            FilterExpr::Term(term) => {
                assert_eq!(
                    term.field.as_ref().unwrap().0,
                    vec!["http", "status"]
                );
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
        let expr =
            parse("level:error AND (service:auth OR service:api) -status:<200").unwrap();
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
        let entry = make_rich_entry();
        let expr = parse("source:web-server").unwrap();
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
        let expr =
            parse("level:error AND (service:auth-api OR service:billing)").unwrap();
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
