/// Parsed frontmatter from a RUNME.rs file.
#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    /// Additional dependencies declared in the frontmatter.
    /// Each entry is `(crate_name, version_or_spec)`.
    pub dependencies: Vec<(String, String)>,
    /// The `name = "..."` value from a `[rnme.rename]` section, if present.
    /// Stored raw (pre-normalization); the consumer substitutes this for the
    /// directory's basename before path-to-ident normalization runs.
    pub rename: Option<String>,
}

/// Tracks which frontmatter section the line walker is currently inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Dependencies,
    RnmeRename,
}

/// Parse the optional frontmatter from RUNME.rs source code.
///
/// Recognizes two sections, in either order:
///
/// ```text
/// //! [dependencies]
/// //! reqwest = "0.12"
/// //! serde_json = "1"
///
/// //! [rnme.rename]
/// //! name = "foo_bar_dashed"
/// ```
///
/// Rules:
/// - Section headers are `//! [<section>]` on their own line.
/// - Subsequent `//! key = "value"` lines belong to the most recently entered
///   section.
/// - A new section header terminates the previous section.
/// - Parsing stops at the first line that is not a `//!` doc comment.
/// - If no frontmatter is found, returns empty dependencies and `rename: None`.
pub fn parse_frontmatter(source: &str) -> Frontmatter {
    let mut dependencies = Vec::new();
    let mut rename: Option<String> = None;
    let mut section = Section::None;

    for line in source.lines() {
        let trimmed = line.trim();

        // Check for doc comment lines
        if let Some(content) = trimmed.strip_prefix("//!") {
            let content = content.trim();

            if content == "[dependencies]" {
                section = Section::Dependencies;
                continue;
            }
            if content == "[rnme.rename]" {
                section = Section::RnmeRename;
                continue;
            }

            match section {
                Section::Dependencies => {
                    // Try to parse as `name = "version"` or `name = { ... }`
                    if let Some(dep) = parse_dependency_line(content) {
                        dependencies.push(dep);
                    }
                    // Continue even if the line doesn't parse -- might be a comment or blank
                }
                Section::RnmeRename => {
                    // Look for `name = "..."`. Forgiving: malformed lines
                    // leave `rename` unchanged (so empty section / unknown key /
                    // non-string value / missing quotes all yield `None`).
                    // Last valid `name = "..."` wins.
                    if let Some(value) = parse_rename_name_line(content) {
                        rename = Some(value);
                    }
                }
                Section::None => {
                    // Doc comment outside any known section -- ignore.
                }
            }
        } else if section != Section::None {
            // First non-`//!` line after entering a section: stop parsing
            break;
        } else if trimmed.is_empty() {
            // Allow blank lines before the frontmatter section
            continue;
        } else {
            // Non-comment, non-blank line before any section: no frontmatter
            break;
        }
    }

    Frontmatter {
        dependencies,
        rename,
    }
}

/// Parse a single `name = "value"` line inside `[rnme.rename]`.
///
/// Returns `Some(value)` only when the line has the form `name = "<non-empty>"`
/// with a properly-closed double-quoted string. Returns `None` for empty
/// values, unquoted bareword values, non-string values (e.g. `name = 42`),
/// missing closing quote, or any other key.
///
/// The returned string is the *raw* value between the quotes — no
/// normalization, no validation against identifier rules.
fn parse_rename_name_line(line: &str) -> Option<String> {
    let eq_pos = line.find('=')?;
    let key = line[..eq_pos].trim();
    if key != "name" {
        return None;
    }
    let value = line[eq_pos + 1..].trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() {
        return None;
    }
    Some(inner.to_string())
}

/// Parse a single dependency line like `reqwest = "0.12"` or
/// `serde = { version = "1", features = ["derive"] }`.
///
/// Returns `(name, value_as_string)` where value_as_string is the raw
/// right-hand side (including quotes or braces).
fn parse_dependency_line(line: &str) -> Option<(String, String)> {
    let eq_pos = line.find('=')?;
    let name = line[..eq_pos].trim();
    let value = line[eq_pos + 1..].trim();

    if name.is_empty() || value.is_empty() {
        return None;
    }

    Some((name.to_string(), value.to_string()))
}

/// Rewrite path dependencies so that relative paths are resolved against the
/// original RUNME.rs file's directory and written as absolute paths.
///
/// For each dependency, if the value contains `path = "..."`, the relative path
/// is resolved against `original_dir`. Dependencies without a `path` key are
/// passed through unchanged.
///
/// Handles both simple string values (`path = "../foo"` parsed as the full
/// value) and inline tables (`{ path = "../foo", features = [...] }`).
pub fn rewrite_path_deps(
    deps: &[(String, String)],
    original_dir: &std::path::Path,
) -> Vec<(String, String)> {
    deps.iter()
        .map(|(name, value)| {
            let rewritten = rewrite_path_in_value(value, original_dir);
            (name.clone(), rewritten)
        })
        .collect()
}

/// Rewrite a `path = "..."` inside a dependency value string.
///
/// If the value doesn't contain a path key, it is returned unchanged.
fn rewrite_path_in_value(value: &str, original_dir: &std::path::Path) -> String {
    // Look for `path = "..."` pattern
    // This handles both:
    //   `{ path = "../foo", features = ["bar"] }` (inline table)
    //   `{ path = "../foo" }` (simple inline table)
    //
    // We use a simple string search rather than a full TOML parser because
    // frontmatter dependency values are simple enough for pattern matching.

    let Some(path_key_start) = find_path_key(value) else {
        return value.to_string();
    };

    // Find the `=` after `path`
    let after_key = &value[path_key_start + 4..]; // skip "path"
    let Some(eq_offset) = after_key.find('=') else {
        return value.to_string();
    };
    let after_eq = &after_key[eq_offset + 1..];
    let after_eq_trimmed = after_eq.trim_start();

    // Find the opening quote
    if !after_eq_trimmed.starts_with('"') {
        return value.to_string();
    }

    // Compute the absolute position of the opening quote in the original string
    let quote_start_in_value =
        value.len() - after_eq.len() + (after_eq.len() - after_eq_trimmed.len());

    // Find the closing quote
    let inner = &value[quote_start_in_value + 1..];
    let Some(close_offset) = inner.find('"') else {
        return value.to_string();
    };

    let rel_path_str = &inner[..close_offset];

    // Check if the path is already absolute
    let rel_path = std::path::Path::new(rel_path_str);
    if rel_path.is_absolute() {
        return value.to_string();
    }

    // Resolve relative to the original RUNME.rs directory
    let resolved = original_dir.join(rel_path);
    // Canonicalize if the path exists, otherwise just use the joined path
    let abs_path = resolved.canonicalize().unwrap_or(resolved);

    // Rebuild the value string with the absolute path
    let before = &value[..quote_start_in_value + 1]; // up to and including opening quote
    let after = &value[quote_start_in_value + 1 + close_offset..]; // from closing quote onward
    format!("{}{}{}", before, abs_path.display(), after)
}

/// Find the start position of the `path` key in a dependency value string.
///
/// Returns `None` if no `path` key is found. This looks for `path` preceded by
/// whitespace, `{`, or start-of-string, followed by whitespace or `=`.
fn find_path_key(value: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(pos) = value[search_from..].find("path") {
        let abs_pos = search_from + pos;

        // Check that it's a key boundary (not part of a larger word)
        let before_ok =
            abs_pos == 0 || matches!(value.as_bytes()[abs_pos - 1], b' ' | b'\t' | b'{' | b',');

        let after_pos = abs_pos + 4;
        let after_ok =
            after_pos >= value.len() || matches!(value.as_bytes()[after_pos], b' ' | b'\t' | b'=');

        if before_ok && after_ok {
            return Some(abs_pos);
        }

        search_from = abs_pos + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_frontmatter() {
        let source = r#"use rnme::prelude::*;

fn hello(ctx: &TaskContext) {
    println!("hello");
}
"#;
        let fm = parse_frontmatter(source);
        assert!(fm.dependencies.is_empty());
        assert_eq!(fm.rename, None);
    }

    #[test]
    fn test_frontmatter_with_deps() {
        let source = r#"//! [dependencies]
//! tokio = "1"

use rnme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.dependencies.len(), 1);
        assert_eq!(
            fm.dependencies[0],
            ("tokio".to_string(), "\"1\"".to_string())
        );
    }

    #[test]
    fn test_frontmatter_with_complex_deps() {
        let source = r#"//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! tokio = { version = "1", features = ["full"] }

use rnme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.dependencies.len(), 2);
        assert_eq!(
            fm.dependencies[0],
            (
                "serde".to_string(),
                "{ version = \"1\", features = [\"derive\"] }".to_string()
            )
        );
    }

    #[test]
    fn test_empty_source() {
        let fm = parse_frontmatter("");
        assert!(fm.dependencies.is_empty());
    }

    #[test]
    fn test_frontmatter_stops_at_non_comment() {
        let source = r#"//! [dependencies]
//! reqwest = "0.12"
use rnme::prelude::*;
//! serde = "1"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.dependencies.len(), 1);
        assert_eq!(
            fm.dependencies[0],
            ("reqwest".to_string(), "\"0.12\"".to_string())
        );
    }

    // --- rewrite_path_deps tests ---

    #[test]
    fn test_rewrite_no_path_deps() {
        let deps = vec![
            ("reqwest".to_string(), "\"0.12\"".to_string()),
            ("serde".to_string(), "\"1\"".to_string()),
        ];
        let original_dir = std::path::Path::new("/home/user/project");
        let result = rewrite_path_deps(&deps, original_dir);
        assert_eq!(result, deps);
    }

    #[test]
    fn test_rewrite_inline_table_with_path() {
        let deps = vec![(
            "my-tools".to_string(),
            "{ path = \"../shared/tools\", features = [\"foo\"] }".to_string(),
        )];
        let original_dir = std::path::Path::new("/home/user/project/services/auth");
        let result = rewrite_path_deps(&deps, original_dir);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "my-tools");
        // The path should be resolved to an absolute path (joined against original_dir)
        assert!(
            result[0]
                .1
                .contains("/home/user/project/services/auth/../shared/tools"),
            "Expected resolved path in: {}",
            result[0].1
        );
        // Features should be preserved
        assert!(
            result[0].1.contains("features = [\"foo\"]"),
            "Expected features preserved in: {}",
            result[0].1
        );
    }

    #[test]
    fn test_rewrite_mixed_deps() {
        let deps = vec![
            ("reqwest".to_string(), "\"0.12\"".to_string()),
            (
                "local-lib".to_string(),
                "{ path = \"../libs/common\" }".to_string(),
            ),
            (
                "serde".to_string(),
                "{ version = \"1\", features = [\"derive\"] }".to_string(),
            ),
        ];
        let original_dir = std::path::Path::new("/home/user/project");
        let result = rewrite_path_deps(&deps, original_dir);

        // reqwest: unchanged
        assert_eq!(result[0].1, "\"0.12\"");
        // local-lib: path rewritten (joined against original_dir)
        assert!(
            result[1].1.contains("/home/user/project/../libs/common"),
            "Expected resolved path in: {}",
            result[1].1
        );
        // serde: unchanged (no path key)
        assert_eq!(result[2].1, "{ version = \"1\", features = [\"derive\"] }");
    }

    #[test]
    fn test_rewrite_already_absolute_path() {
        let deps = vec![(
            "my-tools".to_string(),
            "{ path = \"/absolute/path/to/tools\" }".to_string(),
        )];
        let original_dir = std::path::Path::new("/home/user/project");
        let result = rewrite_path_deps(&deps, original_dir);
        // Should be unchanged since the path is already absolute
        assert_eq!(result[0].1, "{ path = \"/absolute/path/to/tools\" }");
    }

    #[test]
    fn test_rewrite_does_not_match_xpath() {
        // "xpath" contains "path" but is not a path key
        let deps = vec![(
            "xml-thing".to_string(),
            "{ version = \"1\", xpath = \"//foo\" }".to_string(),
        )];
        let original_dir = std::path::Path::new("/home/user/project");
        let result = rewrite_path_deps(&deps, original_dir);
        assert_eq!(result[0].1, deps[0].1);
    }

    #[test]
    fn test_find_path_key_basic() {
        assert_eq!(find_path_key("{ path = \"../foo\" }"), Some(2));
    }

    #[test]
    fn test_find_path_key_not_xpath() {
        assert_eq!(find_path_key("{ xpath = \"//foo\" }"), None);
    }

    #[test]
    fn test_find_path_key_start_of_string() {
        assert_eq!(find_path_key("path = \"../foo\""), Some(0));
    }

    #[test]
    fn test_find_path_key_after_comma() {
        assert!(find_path_key("{ version = \"1\", path = \"../foo\" }").is_some());
    }

    #[test]
    fn test_rewrite_path_with_spaces() {
        // Paths containing spaces should be rewritten correctly
        let deps = vec![(
            "my-lib".to_string(),
            "{ path = \"../my lib/tools\" }".to_string(),
        )];
        let original_dir = std::path::Path::new("/home/user/project");
        let result = rewrite_path_deps(&deps, original_dir);
        assert_eq!(result.len(), 1);
        // The resolved path should contain the joined directory
        assert!(
            result[0].1.contains("/home/user/project"),
            "Expected resolved path in: {}",
            result[0].1
        );
        assert!(
            result[0].1.contains("my lib"),
            "Expected space preserved in path: {}",
            result[0].1
        );
    }

    #[test]
    fn test_rewrite_multiple_path_deps() {
        // Multiple deps that both have path keys should both be rewritten
        let deps = vec![
            ("lib-a".to_string(), "{ path = \"../lib-a\" }".to_string()),
            (
                "lib-b".to_string(),
                "{ path = \"../lib-b\", features = [\"extra\"] }".to_string(),
            ),
        ];
        let original_dir = std::path::Path::new("/home/user/project/services");
        let result = rewrite_path_deps(&deps, original_dir);
        assert_eq!(result.len(), 2);
        assert!(
            result[0].1.contains("/home/user/project/services"),
            "lib-a path not rewritten: {}",
            result[0].1
        );
        assert!(
            result[1].1.contains("/home/user/project/services"),
            "lib-b path not rewritten: {}",
            result[1].1
        );
        assert!(
            result[1].1.contains("features = [\"extra\"]"),
            "lib-b features not preserved: {}",
            result[1].1
        );
    }

    #[test]
    fn test_rewrite_passthrough_no_path_key() {
        // A dep with no path key at all is returned unchanged
        let deps = vec![
            (
                "serde".to_string(),
                "{ version = \"1\", features = [\"derive\"] }".to_string(),
            ),
            ("tokio".to_string(), "\"1\"".to_string()),
        ];
        let original_dir = std::path::Path::new("/home/user/project");
        let result = rewrite_path_deps(&deps, original_dir);
        assert_eq!(result, deps);
    }

    // --- [rnme.rename] parsing tests ---

    #[test]
    fn test_rename_absent() {
        let source = r#"//! [dependencies]
//! tokio = "1"

use rnme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
    }

    #[test]
    fn test_rename_present() {
        let source = r#"//! [rnme.rename]
//! name = "foo_bar_dashed"

use rnme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, Some("foo_bar_dashed".to_string()));
        assert!(fm.dependencies.is_empty());
    }

    #[test]
    fn test_rename_alongside_deps_rename_first() {
        let source = r#"//! [rnme.rename]
//! name = "renamed_thing"
//! [dependencies]
//! tokio = "1"

use rnme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, Some("renamed_thing".to_string()));
        assert_eq!(fm.dependencies.len(), 1);
        assert_eq!(
            fm.dependencies[0],
            ("tokio".to_string(), "\"1\"".to_string())
        );
    }

    #[test]
    fn test_rename_alongside_deps_deps_first() {
        let source = r#"//! [dependencies]
//! tokio = "1"
//! [rnme.rename]
//! name = "renamed_thing"

use rnme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, Some("renamed_thing".to_string()));
        assert_eq!(fm.dependencies.len(), 1);
        assert_eq!(
            fm.dependencies[0],
            ("tokio".to_string(), "\"1\"".to_string())
        );
    }

    #[test]
    fn test_rename_raw_not_normalized() {
        // Spaces, uppercase: preserved exactly. Normalization happens downstream.
        let source = r#"//! [rnme.rename]
//! name = "Hello World"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, Some("Hello World".to_string()));
    }

    #[test]
    fn test_rename_raw_preserves_dashes() {
        let source = r#"//! [rnme.rename]
//! name = "foo-bar"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, Some("foo-bar".to_string()));
    }

    #[test]
    fn test_rename_empty_section() {
        // [rnme.rename] header with no name = ... line yields None.
        let source = r#"//! [rnme.rename]
//! [dependencies]
//! tokio = "1"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
        assert_eq!(fm.dependencies.len(), 1);
    }

    #[test]
    fn test_rename_empty_string() {
        // Empty string treated as absent.
        let source = r#"//! [rnme.rename]
//! name = ""
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
    }

    #[test]
    fn test_rename_missing_quotes() {
        // Bareword value: not a string literal -> ignored.
        let source = r#"//! [rnme.rename]
//! name = foo_bar
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
    }

    #[test]
    fn test_rename_non_string_value() {
        // Non-string value -> ignored.
        let source = r#"//! [rnme.rename]
//! name = 42
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
    }

    #[test]
    fn test_rename_unknown_key() {
        // A non-`name` key in the section -> rename stays None.
        let source = r#"//! [rnme.rename]
//! title = "foo"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
    }

    #[test]
    fn test_rename_last_wins() {
        // Two `name = "..."` lines in the section: last one wins.
        let source = r#"//! [rnme.rename]
//! name = "first"
//! name = "second"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, Some("second".to_string()));
    }

    #[test]
    fn test_rename_stops_at_non_comment() {
        // Section is terminated by a non-`//!` line; it is NOT resumed by a
        // subsequent `//!` line. Consistent with test_frontmatter_stops_at_non_comment.
        let source = r#"//! [rnme.rename]
use rnme::prelude::*;
//! name = "should_not_be_seen"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.rename, None);
    }
}
