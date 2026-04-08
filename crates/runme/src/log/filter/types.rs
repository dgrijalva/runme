use std::fmt;

use regex::Regex;

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
