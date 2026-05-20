/// Transform a RUNME.rs source file for inclusion in a generated workspace crate.
///
/// Transformations applied:
/// 1. Strip `//!` frontmatter lines (already parsed into Cargo.toml)
/// 2. Prepend `const __RNME_GROUP: &str = "<group>";` so the `#[task]` macro can read it
/// 3. Prepend `const __RNME_DIR: &str = "<absolute path>";` so the macro can record
///    the originating RUNME.rs's directory on each `TaskDef`
/// 4. Append `pub fn __rnme_link() {}` — a dummy symbol the runner crate references
///    to prevent the linker from dead-stripping `inventory` registrations
pub fn transform_source(source: &str, group: &str, dir: &str) -> String {
    let without_frontmatter = strip_frontmatter(source);

    // Escape any special characters in the group/dir strings for Rust string
    // literals. Group is path-derived; dir is an OS-derived absolute path —
    // on Windows it'll contain backslashes that must be doubled.
    let escaped_group = escape_rust_str(group);
    let escaped_dir = escape_rust_str(dir);

    format!(
        "const __RNME_GROUP: &str = \"{}\";\nconst __RNME_DIR: &str = \"{}\";\n{}\npub fn __rnme_link() {{}}\n",
        escaped_group, escaped_dir, without_frontmatter
    )
}

fn escape_rust_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        let result = transform_source(source, "services/auth", "/repo/services/auth");
        assert!(result.starts_with("const __RNME_GROUP: &str = \"services/auth\";\n"));
        assert!(result.contains("const __RNME_DIR: &str = \"/repo/services/auth\";\n"));
        assert!(result.contains("use rnme::prelude::*;"));
        assert!(result.ends_with("pub fn __rnme_link() {}\n"));
    }

    #[test]
    fn test_source_with_frontmatter() {
        let source = "//! [dependencies]\n//! reqwest = \"0.12\"\n\nuse rnme::prelude::*;\nuse std::path::Path;\n";
        let result = transform_source(source, ".", "/repo");
        assert!(result.starts_with("const __RNME_GROUP: &str = \".\";\n"));
        assert!(result.contains("const __RNME_DIR: &str = \"/repo\";\n"));
        assert!(!result.contains("//! [dependencies]"));
        assert!(!result.contains("//! reqwest"));
        assert!(result.contains("use rnme::prelude::*;"));
        assert!(result.contains("use std::path::Path;"));
    }

    #[test]
    fn test_group_with_special_chars() {
        let result = transform_source("fn foo() {}\n", "path/with\"quotes", "/d");
        assert!(result.contains(r#"const __RNME_GROUP: &str = "path/with\"quotes";"#));
    }

    #[test]
    fn test_group_with_backslash() {
        let result = transform_source("fn foo() {}\n", r"path\back", "/d");
        assert!(result.contains(r#"const __RNME_GROUP: &str = "path\\back";"#));
    }

    #[test]
    fn test_dir_with_backslash() {
        let result = transform_source("fn foo() {}\n", "g", r"C:\repo\sub");
        assert!(result.contains(r#"const __RNME_DIR: &str = "C:\\repo\\sub";"#));
    }

    #[test]
    fn test_empty_source() {
        let result = transform_source("", "root", "/root");
        assert_eq!(
            result,
            "const __RNME_GROUP: &str = \"root\";\nconst __RNME_DIR: &str = \"/root\";\n\npub fn __rnme_link() {}\n"
        );
    }

    #[test]
    fn test_root_group() {
        let source = "fn main() {}\n";
        let result = transform_source(source, "", "/d");
        assert!(result.starts_with("const __RNME_GROUP: &str = \"\";\n"));
    }

    #[test]
    fn test_link_function_present() {
        let result = transform_source("fn foo() {}\n", "test", "/d");
        assert!(result.contains("pub fn __rnme_link() {}"));
    }

    #[test]
    fn test_frontmatter_stripped() {
        let source = "//! [dependencies]\n//! tokio = \"1\"\n\nfn work() {}\n";
        let result = transform_source(source, "infra", "/repo/infra");
        assert!(result.starts_with("const __RNME_GROUP: &str = \"infra\";\n"));
        assert!(!result.contains("//! [dependencies]"));
        assert!(!result.contains("//! tokio"));
        assert!(result.contains("fn work() {}"));
        assert!(result.ends_with("pub fn __rnme_link() {}\n"));
    }

    #[test]
    fn test_idempotent_double_transform() {
        let source = "fn task_a() {}\n";
        let once = transform_source(source, "group_a", "/a");
        let twice = transform_source(&once, "group_b", "/b");
        assert!(twice.starts_with("const __RNME_GROUP: &str = \"group_b\";\n"));
        assert!(twice.contains("const __RNME_GROUP: &str = \"group_a\";"));
    }

    #[test]
    fn test_group_with_quotes_and_backslash() {
        let result = transform_source("fn f() {}\n", "foo\\\"bar", "/d");
        assert!(result.contains(r#"const __RNME_GROUP: &str = "foo\\\"bar";"#));
    }
}
