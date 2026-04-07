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

    // Escape any special characters in the group string for a Rust string literal.
    // In practice group strings are path-derived (e.g. "services/auth") but be safe.
    let escaped_group = group.replace('\\', "\\\\").replace('"', "\\\"");

    format!(
        "const __RUNME_GROUP: &str = \"{}\";\n{}\npub fn __runme_link() {{}}\n",
        escaped_group, stripped
    )
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
        assert!(result.contains("//! [dependencies]"));
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
}
