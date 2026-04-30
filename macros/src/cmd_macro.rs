use proc_macro2::{Delimiter, LineColumn, Span, TokenStream, TokenTree};
use quote::quote;

/// A parsed argument in the cmd! macro.
#[allow(dead_code)] // Span kept for future error reporting
enum CmdArg {
    /// A literal string assembled from adjacent tokens or a string literal.
    Literal(String, Span),
    /// An interpolated expression from `{expr}`.
    Expr(TokenStream, Span),
}

/// Parse cmd! input and generate a `Cmd` expression.
///
/// Tokenization rules:
/// - Whitespace separates arguments (detected via span positions)
/// - `{expr}` braces create an interpolation argument
/// - `"quoted strings"` are single literal arguments (unquoted content)
/// - Adjacent tokens (no whitespace) concatenate into one argument
/// - First argument becomes the program: `Cmd::new(...)`
/// - Remaining arguments become `.arg(...)` calls
pub(crate) fn expand_cmd(input: TokenStream) -> Result<TokenStream, syn::Error> {
    let args = parse_args(input)?;

    if args.is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "cmd! requires at least a program name",
        ));
    }

    let mut chain = match &args[0] {
        CmdArg::Literal(s, _) => quote! { ::rnme::cmd::Cmd::new(#s) },
        CmdArg::Expr(expr, _) => quote! { ::rnme::cmd::Cmd::new(#expr) },
    };

    for arg in &args[1..] {
        chain = match arg {
            CmdArg::Literal(s, _) => quote! { #chain.arg(#s) },
            CmdArg::Expr(expr, _) => quote! { #chain.arg(#expr) },
        };
    }

    Ok(chain)
}

fn parse_args(input: TokenStream) -> Result<Vec<CmdArg>, syn::Error> {
    let mut args = Vec::new();
    let mut current_word = String::new();
    let mut word_span: Option<Span> = None;
    let mut last_end: Option<LineColumn> = None;

    for tt in input {
        match &tt {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                flush_word(&mut args, &mut current_word, &mut word_span);
                last_end = Some(g.span().end());
                args.push(CmdArg::Expr(g.stream(), g.span()));
            }
            TokenTree::Group(g) => {
                return Err(syn::Error::new(
                    g.span(),
                    "unexpected group in cmd! — use {expr} for interpolation",
                ));
            }
            TokenTree::Literal(lit) => {
                let repr = lit.to_string();
                let is_string =
                    repr.starts_with('"') || repr.starts_with("r\"") || repr.starts_with("r#");

                if is_string {
                    flush_word(&mut args, &mut current_word, &mut word_span);
                    last_end = Some(lit.span().end());
                    let s: syn::LitStr = syn::parse2(TokenStream::from(tt.clone()))?;
                    args.push(CmdArg::Literal(s.value(), lit.span()));
                } else {
                    let start = lit.span().start();
                    if !is_adjacent(last_end, start) {
                        flush_word(&mut args, &mut current_word, &mut word_span);
                    }
                    last_end = Some(lit.span().end());
                    if word_span.is_none() {
                        word_span = Some(lit.span());
                    }
                    current_word.push_str(&repr);
                }
            }
            TokenTree::Ident(ident) => {
                let start = ident.span().start();
                if !is_adjacent(last_end, start) {
                    flush_word(&mut args, &mut current_word, &mut word_span);
                }
                last_end = Some(ident.span().end());
                if word_span.is_none() {
                    word_span = Some(ident.span());
                }
                current_word.push_str(&ident.to_string());
            }
            TokenTree::Punct(punct) => {
                let start = punct.span().start();
                if !is_adjacent(last_end, start) {
                    flush_word(&mut args, &mut current_word, &mut word_span);
                }
                last_end = Some(punct.span().end());
                if word_span.is_none() {
                    word_span = Some(punct.span());
                }
                current_word.push(punct.as_char());
            }
        }
    }

    flush_word(&mut args, &mut current_word, &mut word_span);
    Ok(args)
}

fn flush_word(args: &mut Vec<CmdArg>, word: &mut String, span: &mut Option<Span>) {
    if !word.is_empty() {
        args.push(CmdArg::Literal(
            std::mem::take(word),
            span.take().unwrap_or_else(Span::call_site),
        ));
    }
    *span = None;
}

/// Two tokens are adjacent when the first ends exactly where the second starts.
fn is_adjacent(last_end: Option<LineColumn>, start: LineColumn) -> bool {
    match last_end {
        None => true,
        Some(end) => end.line == start.line && end.column == start.column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_arg_strings(input: &str) -> Vec<String> {
        let ts: TokenStream = input.parse().unwrap();
        let args = parse_args(ts).unwrap();
        args.into_iter()
            .map(|a| match a {
                CmdArg::Literal(s, _) => format!("lit:{s}"),
                CmdArg::Expr(ts, _) => format!("expr:{ts}"),
            })
            .collect()
    }

    #[test]
    fn simple_command() {
        assert_eq!(
            parse_arg_strings("curl -X POST"),
            vec!["lit:curl", "lit:-X", "lit:POST"]
        );
    }

    #[test]
    fn double_dash_flag() {
        assert_eq!(
            parse_arg_strings("cargo build --release"),
            vec!["lit:cargo", "lit:build", "lit:--release"]
        );
    }

    #[test]
    fn string_literal_arg() {
        assert_eq!(
            parse_arg_strings(r#"curl -H "Content-Type: application/json""#),
            vec!["lit:curl", "lit:-H", "lit:Content-Type: application/json"]
        );
    }

    #[test]
    fn interpolation() {
        let result = parse_arg_strings("curl -X POST {&url}");
        assert_eq!(result[..3], ["lit:curl", "lit:-X", "lit:POST"]);
        assert!(result[3].starts_with("expr:"));
    }

    #[test]
    fn mixed_literal_and_interpolation() {
        let result = parse_arg_strings(
            r#"curl -X POST {&url} -H "Content-Type: multipart/form-data" -F {&path}"#,
        );
        assert_eq!(result[0], "lit:curl");
        assert_eq!(result[1], "lit:-X");
        assert_eq!(result[2], "lit:POST");
        assert!(result[3].starts_with("expr:"));
        assert_eq!(result[4], "lit:-H");
        assert_eq!(result[5], "lit:Content-Type: multipart/form-data");
        assert_eq!(result[6], "lit:-F");
        assert!(result[7].starts_with("expr:"));
    }

    #[test]
    fn empty_input() {
        let ts: TokenStream = "".parse().unwrap();
        assert!(expand_cmd(ts).is_err());
    }

    #[test]
    fn numeric_arg() {
        assert_eq!(parse_arg_strings("echo 42"), vec!["lit:echo", "lit:42"]);
    }

    #[test]
    fn equals_in_flag() {
        assert_eq!(
            parse_arg_strings("cargo build --jobs=4"),
            vec!["lit:cargo", "lit:build", "lit:--jobs=4"]
        );
    }

    #[test]
    fn path_like_arg() {
        assert_eq!(
            parse_arg_strings("ls src/main.rs"),
            vec!["lit:ls", "lit:src/main.rs"]
        );
    }
}
