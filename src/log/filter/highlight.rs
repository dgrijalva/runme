//! Extracting highlightable substrings from a `FilterExpr`.
//!
//! When a filter is active, we want to highlight the substrings in matched
//! entries that contributed to a positive value match. This module walks the
//! expression tree, respects nested negation (`NOT` and `term.negated`), and
//! collects the string-valued matchers that effectively match positively.

use super::types::{FilterExpr, Matcher};

/// Walk a filter expression and collect the string literals from positive
/// value matchers. Negated terms (anywhere under a `Not`, or with their own
/// `term.negated` flag) are skipped because they wouldn't appear in the
/// matching entries' text in a meaningful way.
///
/// Numeric comparisons and regex matchers are skipped (Substring/Exact/
/// Wildcard string literals are the only kinds returned).
///
/// The result may contain duplicates; callers can dedupe if needed.
pub fn collect_positive_literals(expr: &FilterExpr) -> Vec<String> {
    let mut out = Vec::new();
    walk(expr, false, &mut out);
    out
}

fn walk(expr: &FilterExpr, negated: bool, out: &mut Vec<String>) {
    match expr {
        FilterExpr::And(a, b) | FilterExpr::Or(a, b) => {
            walk(a, negated, out);
            walk(b, negated, out);
        }
        FilterExpr::Not(inner) => {
            walk(inner, !negated, out);
        }
        FilterExpr::Term(term) => {
            let effective_negated = negated ^ term.negated;
            if effective_negated {
                return;
            }
            match &term.matcher {
                Matcher::Substring(s) | Matcher::Exact(s) | Matcher::Wildcard(s) => {
                    if !s.is_empty() {
                        out.push(s.clone());
                    }
                }
                Matcher::Regex(_) | Matcher::Comparison(_, _) => {
                    // No simple substring to highlight.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use super::*;

    fn literals(src: &str) -> Vec<String> {
        let expr = parse(src).expect("parse");
        collect_positive_literals(&expr)
    }

    #[test]
    fn bare_literal() {
        assert_eq!(literals("error"), vec!["error".to_string()]);
    }

    #[test]
    fn field_value_match() {
        // `level:error` — the value "error" should highlight.
        assert_eq!(literals("level:error"), vec!["error".to_string()]);
    }

    #[test]
    fn conjunction_collects_both() {
        let mut got = literals("level:error AND timeout");
        got.sort();
        assert_eq!(got, vec!["error".to_string(), "timeout".to_string()]);
    }

    #[test]
    fn negation_excludes() {
        // `NOT foo` — nothing to highlight.
        assert!(literals("NOT foo").is_empty());
    }

    #[test]
    fn negated_term_excludes() {
        // `-foo` (term-level negation) — nothing to highlight.
        assert!(literals("-foo").is_empty());
    }

    #[test]
    fn double_negation_includes() {
        // `NOT NOT foo` — back to positive.
        assert_eq!(literals("NOT NOT foo"), vec!["foo".to_string()]);
    }

    #[test]
    fn mixed_negation_only_positives() {
        // `error AND NOT debug` — only "error" highlights.
        let got = literals("error AND NOT debug");
        assert_eq!(got, vec!["error".to_string()]);
    }
}
