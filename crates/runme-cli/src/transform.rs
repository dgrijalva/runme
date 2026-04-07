use crate::frontmatter::strip_shebang;

/// Transform a RUNME.rs source file for inclusion in a generated workspace crate.
///
/// Transformations applied:
/// 1. Strip the shebang line (`#!/usr/bin/env runme`)
/// 2. Prepend `const __RUNME_GROUP: &str = "<group>";` so the `#[task]` macro can read it
/// 3. Append `pub fn __runme_link() {}` — a dummy symbol the runner crate references
///    to prevent the linker from dead-stripping `inventory` registrations
pub fn transform_source(source: &str, group: &str) -> String {
    let stripped = strip_shebang(source);

    // Strip //! frontmatter lines — they've already been parsed into Cargo.toml
    // by the frontmatter module. Leaving them in causes compiler errors since
    // they appear after the injected const.
    let without_frontmatter = strip_frontmatter(&stripped);

    // Escape any special characters in the group string for a Rust string literal.
    // In practice group strings are path-derived (e.g. "services/auth") but be safe.
    let escaped_group = group.replace('\\', "\\\\").replace('"', "\\\"");

    format!(
        "const __RUNME_GROUP: &str = \"{}\";\n{}\npub fn __runme_link() {{}}\n",
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
    fn test_source_with_shebang() {
        let source = "#!/usr/bin/env runme\nuse runme::prelude::*;\n\nfn hello() {}\n";
        let result = transform_source(source, "services/auth");
        assert!(result.starts_with("const __RUNME_GROUP: &str = \"services/auth\";\n"));
        assert!(result.contains("use runme::prelude::*;"));
        assert!(!result.contains("#!/usr/bin/env runme"));
        assert!(result.ends_with("pub fn __runme_link() {}\n"));
    }

    #[test]
    fn test_source_without_shebang() {
        let source = "use runme::prelude::*;\n\nfn hello() {}\n";
        let result = transform_source(source, "web");
        assert!(result.starts_with("const __RUNME_GROUP: &str = \"web\";\n"));
        assert!(result.contains("use runme::prelude::*;"));
        assert!(result.ends_with("pub fn __runme_link() {}\n"));
    }

    #[test]
    fn test_source_with_imports() {
        let source = "#!/usr/bin/env runme\n//! [dependencies]\n//! reqwest = \"0.12\"\n\nuse runme::prelude::*;\nuse std::path::Path;\n";
        let result = transform_source(source, ".");
        assert!(result.starts_with("const __RUNME_GROUP: &str = \".\";\n"));
        assert!(!result.contains("//! [dependencies]"));
        assert!(!result.contains("//! reqwest"));
        assert!(result.contains("use runme::prelude::*;"));
        assert!(result.contains("use std::path::Path;"));
    }

    #[test]
    fn test_group_with_special_chars() {
        let result = transform_source("fn foo() {}\n", "path/with\"quotes");
        assert!(result.contains(r#"const __RUNME_GROUP: &str = "path/with\"quotes";"#));
    }

    #[test]
    fn test_group_with_backslash() {
        let result = transform_source("fn foo() {}\n", r"path\back");
        assert!(result.contains(r#"const __RUNME_GROUP: &str = "path\\back";"#));
    }

    #[test]
    fn test_empty_source() {
        let result = transform_source("", "root");
        assert_eq!(
            result,
            "const __RUNME_GROUP: &str = \"root\";\n\npub fn __runme_link() {}\n"
        );
    }

    #[test]
    fn test_root_group() {
        let source = "fn main() {}\n";
        let result = transform_source(source, "");
        assert!(result.starts_with("const __RUNME_GROUP: &str = \"\";\n"));
    }

    #[test]
    fn test_link_function_present() {
        let result = transform_source("fn foo() {}\n", "test");
        assert!(result.contains("pub fn __runme_link() {}"));
    }

    #[test]
    fn test_shebang_and_frontmatter_comments_both_present() {
        // Both shebang and //! frontmatter are stripped from transformed output.
        let source = "#!/usr/bin/env runme\n//! [dependencies]\n//! tokio = \"1\"\n\nfn work() {}\n";
        let result = transform_source(source, "infra");
        assert!(result.starts_with("const __RUNME_GROUP: &str = \"infra\";\n"));
        assert!(!result.contains("#!/usr/bin/env runme"));
        assert!(!result.contains("//! [dependencies]"));
        assert!(!result.contains("//! tokio"));
        assert!(result.contains("fn work() {}"));
        assert!(result.ends_with("pub fn __runme_link() {}\n"));
    }

    #[test]
    fn test_idempotent_double_transform() {
        // Applying transform_source twice should still produce a compilable-ish
        // result — the outer __RUNME_GROUP constant will come first, so the
        // second application does NOT collapse them; just verify no panic and
        // that the outermost constant reflects the second group argument.
        let source = "fn task_a() {}\n";
        let once = transform_source(source, "group_a");
        let twice = transform_source(&once, "group_b");
        // The outermost constant should reflect group_b
        assert!(twice.starts_with("const __RUNME_GROUP: &str = \"group_b\";\n"));
        // The inner constant from the first pass is still present
        assert!(twice.contains("const __RUNME_GROUP: &str = \"group_a\";"));
    }

    #[test]
    fn test_group_with_quotes_and_backslash() {
        // Both escaping types in a single group string
        let result = transform_source("fn f() {}\n", "foo\\\"bar");
        assert!(result.contains(r#"const __RUNME_GROUP: &str = "foo\\\"bar";"#));
    }
}
