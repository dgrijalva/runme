use regex::Regex;
use winnow::Parser;
use winnow::combinator::{alt, opt};
use winnow::error::{ContextError, StrContext, StrContextValue};
use winnow::token::{any, take_while};

use super::types::*;

type PResult<T> = winnow::Result<T, ContextError>;

/// Parse a filter expression string into an AST.
///
/// Returns an error with a descriptive message if the input is malformed.
pub fn parse(input: &str) -> Result<FilterExpr, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty filter expression".to_string());
    }
    let mut stream = input;
    query
        .parse_next(&mut stream)
        .map_err(|e| format!("parse error: {}", e))
        .and_then(|expr| {
            let remaining = stream.trim();
            if remaining.is_empty() {
                Ok(expr)
            } else {
                Err(format!("unexpected trailing input: {:?}", remaining))
            }
        })
}

/// Skip horizontal whitespace.
fn ws(input: &mut &str) -> PResult<()> {
    take_while(0.., ' ').void().parse_next(input)
}

/// Try to consume a keyword with word-boundary check. Returns true if consumed.
fn try_keyword(input: &mut &str, kw: &str) -> bool {
    if let Some(rest) = input.strip_prefix(kw)
        && (rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    {
        *input = rest;
        return true;
    }
    false
}

/// Peek at whether a keyword is next (without consuming).
fn peek_keyword(input: &str, kw: &str) -> bool {
    input.strip_prefix(kw).is_some_and(|rest| {
        rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
    })
}

/// Parse an escaped string body up to `delimiter`, handling `\x` escapes.
fn escaped_body(input: &mut &str, delimiter: char) -> PResult<String> {
    let mut result = String::new();
    loop {
        let chunk: &str =
            take_while(0.., |c: char| c != delimiter && c != '\\').parse_next(input)?;
        result.push_str(chunk);
        if input.is_empty() || input.starts_with(delimiter) {
            break;
        }
        // Consume backslash + next char
        '\\'.parse_next(input)?;
        if !input.is_empty() {
            result.push(any.parse_next(input)?);
        }
    }
    Ok(result)
}

// -- Leaf parsers (values) --------------------------------------------------

/// Parse a comparison operator: `>=`, `>`, `<=`, `<`.
fn cmp_op(input: &mut &str) -> PResult<CmpOp> {
    alt((
        ">=".value(CmpOp::Gte),
        "<=".value(CmpOp::Lte),
        ">".value(CmpOp::Gt),
        "<".value(CmpOp::Lt),
    ))
    .parse_next(input)
}

/// Parse a comparison value: `>400`, `>=3.5`, `<-1`.
fn comparison(input: &mut &str) -> PResult<Matcher> {
    let op = cmp_op.parse_next(input)?;
    let num_str: &str = take_while(1.., |c: char| c.is_ascii_digit() || c == '.' || c == '-')
        .context(StrContext::Expected(StrContextValue::Description(
            "number after comparison operator",
        )))
        .parse_next(input)?;
    let num: f64 = num_str.parse().map_err(|_| {
        let mut e = ContextError::new();
        e.push(StrContext::Expected(StrContextValue::Description(
            "valid number",
        )));
        e
    })?;
    Ok(Matcher::Comparison(op, num))
}

/// Parse a regex value: `/pattern/`.
fn regex_value(input: &mut &str) -> PResult<Matcher> {
    '/'.parse_next(input)?;
    let pattern = escaped_body(input, '/')?;
    '/'.context(StrContext::Expected(StrContextValue::Description(
        "closing '/'",
    )))
    .parse_next(input)?;
    Regex::new(&pattern).map(Matcher::Regex).map_err(|_| {
        let mut e = ContextError::new();
        e.push(StrContext::Expected(StrContextValue::Description(
            "valid regex pattern",
        )));
        e
    })
}

/// Parse a quoted string: `"..."`.
fn quoted_string(input: &mut &str) -> PResult<Matcher> {
    '"'.parse_next(input)?;
    let value = escaped_body(input, '"')?;
    '"'.context(StrContext::Expected(StrContextValue::Description(
        "closing '\"'",
    )))
    .parse_next(input)?;
    Ok(Matcher::Exact(value))
}

/// Parse a bare word (non-space, non-paren chars). Returns the word as a Matcher.
fn bare_value(input: &mut &str) -> PResult<Matcher> {
    let word: &str =
        take_while(1.., |c: char| c != ' ' && c != ')' && c != '(').parse_next(input)?;
    Ok(if word.contains('*') || word.contains('?') {
        Matcher::Wildcard(word.to_string())
    } else {
        Matcher::Substring(word.to_string())
    })
}

/// Parse a value after `field:` — first character determines the value type.
fn value_matcher(input: &mut &str) -> PResult<Matcher> {
    match input.chars().next() {
        None => Err(ContextError::new()),
        Some('>' | '<') => comparison(input),
        Some('/') => regex_value(input),
        Some('"') => quoted_string(input),
        Some(_) => bare_value(input),
    }
}

// -- Term parser ------------------------------------------------------------

/// Characters valid in a field name.
fn is_field_char(c: char) -> bool {
    c.is_alphanumeric() || "_.-@".contains(c)
}

/// Parse a single term: `'-'? field ':' value` or `'-'? bare_text`.
fn term_expr(input: &mut &str) -> PResult<FilterExpr> {
    ws.parse_next(input)?;
    let negated = opt('-').parse_next(input)?.is_some();
    let saved = *input;

    // Try field:value — scan for colon within field-valid characters
    let field_name: &str = take_while(0.., is_field_char).parse_next(input)?;
    if !field_name.is_empty()
        && !matches!(field_name, "AND" | "OR" | "NOT")
        && opt(':').parse_next(input)?.is_some()
    {
        let field = FieldPath(field_name.split('.').map(String::from).collect());
        let matcher = value_matcher
            .context(StrContext::Label("value after ':'"))
            .parse_next(input)?;
        return Ok(FilterExpr::Term(FilterTerm {
            negated,
            field: Some(field),
            matcher,
        }));
    }

    // Not field:value — backtrack and parse as bare text
    *input = saved;
    let matcher = bare_value.parse_next(input)?;
    Ok(FilterExpr::Term(FilterTerm {
        negated,
        field: None,
        matcher,
    }))
}

// -- Expression parsers (precedence climbing) -------------------------------

/// NOT prefix, parenthesized group, or term.
fn unary_expr(input: &mut &str) -> PResult<FilterExpr> {
    ws.parse_next(input)?;
    if try_keyword(input, "NOT") {
        ws.parse_next(input)?;
        let inner = unary_expr.parse_next(input)?;
        return Ok(FilterExpr::Not(Box::new(inner)));
    }
    if opt('(').parse_next(input)?.is_some() {
        ws.parse_next(input)?;
        let inner = query.parse_next(input)?;
        ws.parse_next(input)?;
        ')'.context(StrContext::Expected(StrContextValue::CharLiteral(')')))
            .parse_next(input)?;
        return Ok(inner);
    }
    term_expr.parse_next(input)
}

/// AND precedence (explicit `AND` or implicit juxtaposition).
fn and_expr(input: &mut &str) -> PResult<FilterExpr> {
    let mut left = unary_expr.parse_next(input)?;
    loop {
        ws.parse_next(input)?;
        if input.is_empty() || input.starts_with(')') || peek_keyword(input, "OR") {
            break;
        }
        let _ = try_keyword(input, "AND"); // optional explicit AND
        ws.parse_next(input)?;
        if input.is_empty() || input.starts_with(')') || peek_keyword(input, "OR") {
            break;
        }
        let right = unary_expr.parse_next(input)?;
        left = FilterExpr::And(Box::new(left), Box::new(right));
    }
    Ok(left)
}

/// OR precedence: `and_expr ("OR" and_expr)*`.
fn or_expr(input: &mut &str) -> PResult<FilterExpr> {
    let mut left = and_expr.parse_next(input)?;
    loop {
        ws.parse_next(input)?;
        if !try_keyword(input, "OR") {
            break;
        }
        ws.parse_next(input)?;
        let right = and_expr
            .context(StrContext::Label("expression after OR"))
            .parse_next(input)?;
        left = FilterExpr::Or(Box::new(left), Box::new(right));
    }
    Ok(left)
}

/// Top-level query entry point.
fn query(input: &mut &str) -> PResult<FilterExpr> {
    or_expr.parse_next(input)
}
