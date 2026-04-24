use regex::Regex;

use super::types::*;
use crate::log::LogEntry;

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
