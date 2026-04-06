/// Parsed frontmatter from a RUNME.rs file.
#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    /// Additional dependencies declared in the frontmatter.
    /// Each entry is `(crate_name, version_or_spec)`.
    pub dependencies: Vec<(String, String)>,
}

/// Parse the optional dependency frontmatter from RUNME.rs source code.
///
/// Looks for a section like:
/// ```text
/// #!/usr/bin/env runme
/// //! [dependencies]
/// //! reqwest = "0.12"
/// //! serde_json = "1"
/// ```
///
/// Rules:
/// - Shebang line (`#!...`) is skipped
/// - `//! [dependencies]` starts the dependencies section
/// - Subsequent `//! name = "version"` lines are parsed as dependency declarations
/// - Parsing stops at the first line that is not a `//!` doc comment
/// - If no frontmatter is found, returns empty dependencies
pub fn parse_frontmatter(source: &str) -> Frontmatter {
    let mut dependencies = Vec::new();
    let mut in_deps_section = false;

    for line in source.lines() {
        let trimmed = line.trim();

        // Skip shebang
        if trimmed.starts_with("#!") && !trimmed.starts_with("//") {
            continue;
        }

        // Check for doc comment lines
        if let Some(content) = trimmed.strip_prefix("//!") {
            let content = content.trim();

            if content == "[dependencies]" {
                in_deps_section = true;
                continue;
            }

            if in_deps_section {
                // Try to parse as `name = "version"` or `name = { ... }`
                if let Some(dep) = parse_dependency_line(content) {
                    dependencies.push(dep);
                }
                // Continue even if the line doesn't parse -- might be a comment or blank
            }
        } else if in_deps_section {
            // First non-`//!` line after entering deps section: stop parsing
            break;
        } else if trimmed.is_empty() {
            // Allow blank lines before the frontmatter section
            continue;
        } else {
            // Non-comment, non-blank line before deps section: no frontmatter
            break;
        }
    }

    Frontmatter { dependencies }
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

/// Strip the shebang line from source code, if present.
pub fn strip_shebang(source: &str) -> &str {
    if source.starts_with("#!") {
        // Find the end of the first line
        match source.find('\n') {
            Some(pos) => &source[pos + 1..],
            None => "", // entire file is just a shebang
        }
    } else {
        source
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_frontmatter() {
        let source = r#"use runme::prelude::*;

fn hello(ctx: &TaskContext) {
    println!("hello");
}
"#;
        let fm = parse_frontmatter(source);
        assert!(fm.dependencies.is_empty());
    }

    #[test]
    fn test_frontmatter_with_shebang() {
        let source = r#"#!/usr/bin/env runme
//! [dependencies]
//! reqwest = "0.12"
//! serde_json = "1"

use runme::prelude::*;
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.dependencies.len(), 2);
        assert_eq!(
            fm.dependencies[0],
            ("reqwest".to_string(), "\"0.12\"".to_string())
        );
        assert_eq!(
            fm.dependencies[1],
            ("serde_json".to_string(), "\"1\"".to_string())
        );
    }

    #[test]
    fn test_frontmatter_without_shebang() {
        let source = r#"//! [dependencies]
//! tokio = "1"

use runme::prelude::*;
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
        let source = r#"#!/usr/bin/env runme
//! [dependencies]
//! serde = { version = "1", features = ["derive"] }
//! tokio = { version = "1", features = ["full"] }

use runme::prelude::*;
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
    fn test_strip_shebang_present() {
        let source = "#!/usr/bin/env runme\nuse runme::prelude::*;\n";
        assert_eq!(strip_shebang(source), "use runme::prelude::*;\n");
    }

    #[test]
    fn test_strip_shebang_absent() {
        let source = "use runme::prelude::*;\n";
        assert_eq!(strip_shebang(source), source);
    }

    #[test]
    fn test_strip_shebang_only() {
        let source = "#!/usr/bin/env runme";
        assert_eq!(strip_shebang(source), "");
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
use runme::prelude::*;
//! serde = "1"
"#;
        let fm = parse_frontmatter(source);
        assert_eq!(fm.dependencies.len(), 1);
        assert_eq!(
            fm.dependencies[0],
            ("reqwest".to_string(), "\"0.12\"".to_string())
        );
    }
}
