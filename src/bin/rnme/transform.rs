/// Transform a RUNME.rs source file for inclusion in a generated workspace crate.
///
/// Transformations applied:
/// 1. Strip `//!` frontmatter lines (already parsed into Cargo.toml)
/// 2. Prepend `const __RNME_GROUP: &str = "<group>";` so the `#[task]` macro can read it
/// 3. Append `pub fn __rnme_link() {}` — a dummy symbol the runner crate references
///    to prevent the linker from dead-stripping `inventory` registrations
pub fn transform_source(source: &str, group: &str) -> String {
    let without_frontmatter = strip_frontmatter(source);

    // Escape any special characters in the group string for a Rust string literal.
    // In practice group strings are path-derived (e.g. "services/auth") but be safe.
    let escaped_group = group.replace('\\', "\\\\").replace('"', "\\\"");

    format!(
        "const __RNME_GROUP: &str = \"{}\";\n{}\npub fn __rnme_link() {{}}\n",
        escaped_group, without_frontmatter
    )
}

/// Strip leading `//!` doc comment lines (frontmatter) from source.
fn strip_frontmatter(source: &str) -> &str {
    let mut rest = source;
    loop {
        let trimmed = rest.trim_start_matches('\n');
        if trimmed.starts_with("//!") {
            // Skip this line
            rest = match trimmed.find('\n') {
                Some(pos) => &trimmed[pos + 1..],
                None => "",
            };
        } else {
            return trimmed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_transform() {
        let source = "use rnme::prelude::*;\n\nfn hello() {}\n";
        let result = transform_source(source, "services/auth");
        assert!(result.starts_with("const __RNME_GROUP: &str = \"services/auth\";\n"));
        assert!(result.contains("use rnme::prelude::*;"));
        assert!(result.ends_with("pub fn __rnme_link() {}\n"));
    }

    #[test]
    fn test_source_with_frontmatter() {
        let source = "//! [dependencies]\n//! reqwest = \"0.12\"\n\nuse rnme::prelude::*;\nuse std::path::Path;\n";
        let result = transform_source(source, ".");
        assert!(result.starts_with("const __RNME_GROUP: &str = \".\";\n"));
        assert!(!result.contains("//! [dependencies]"));
        assert!(!result.contains("//! reqwest"));
        assert!(result.contains("use rnme::prelude::*;"));
        assert!(result.contains("use std::path::Path;"));
    }

    #[test]
    fn test_group_with_special_chars() {
        let result = transform_source("fn foo() {}\n", "path/with\"quotes");
        assert!(result.contains(r#"const __RNME_GROUP: &str = "path/with\"quotes";"#));
    }

    #[test]
    fn test_group_with_backslash() {
        let result = transform_source("fn foo() {}\n", r"path\back");
        assert!(result.contains(r#"const __RNME_GROUP: &str = "path\\back";"#));
    }

    #[test]
    fn test_empty_source() {
        let result = transform_source("", "root");
        assert_eq!(
            result,
            "const __RNME_GROUP: &str = \"root\";\n\npub fn __rnme_link() {}\n"
        );
    }

    #[test]
    fn test_root_group() {
        let source = "fn main() {}\n";
        let result = transform_source(source, "");
        assert!(result.starts_with("const __RNME_GROUP: &str = \"\";\n"));
    }

    #[test]
    fn test_link_function_present() {
        let result = transform_source("fn foo() {}\n", "test");
        assert!(result.contains("pub fn __rnme_link() {}"));
    }

    #[test]
    fn test_frontmatter_stripped() {
        let source =
            "//! [dependencies]\n//! tokio = \"1\"\n\nfn work() {}\n";
        let result = transform_source(source, "infra");
        assert!(result.starts_with("const __RNME_GROUP: &str = \"infra\";\n"));
        assert!(!result.contains("//! [dependencies]"));
        assert!(!result.contains("//! tokio"));
        assert!(result.contains("fn work() {}"));
        assert!(result.ends_with("pub fn __rnme_link() {}\n"));
    }

    #[test]
    fn test_idempotent_double_transform() {
        let source = "fn task_a() {}\n";
        let once = transform_source(source, "group_a");
        let twice = transform_source(&once, "group_b");
        assert!(twice.starts_with("const __RNME_GROUP: &str = \"group_b\";\n"));
        assert!(twice.contains("const __RNME_GROUP: &str = \"group_a\";"));
    }

    #[test]
    fn test_group_with_quotes_and_backslash() {
        let result = transform_source("fn f() {}\n", "foo\\\"bar");
        assert!(result.contains(r#"const __RNME_GROUP: &str = "foo\\\"bar";"#));
    }
}
