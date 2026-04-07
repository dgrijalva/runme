use std::collections::HashMap;
use std::path::Path;

/// Rust keywords that cannot be used as crate names.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "yield",
];

/// Convert a relative path (to a RUNME.rs file) into a valid Rust crate name.
///
/// Rules:
/// - Strip the `RUNME.rs` filename component
/// - If the remaining path is empty (root), return `"root"`
/// - Replace `/`, `-`, `.` with `_`
/// - Trim trailing `_`
/// - Prefix with `runme_` if the result is a Rust keyword or starts with a digit
pub fn crate_name_from_path(rel_path: &Path) -> String {
    // Strip the RUNME.rs filename
    let dir = rel_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // Normalize: strip leading `./`
    let dir = dir.strip_prefix("./").unwrap_or(&dir);

    // Empty dir means root
    if dir.is_empty() || dir == "." {
        return "root".to_string();
    }

    // Replace /, -, . with _
    let name: String = dir
        .chars()
        .map(|c| match c {
            '/' | '-' | '.' => '_',
            _ => c,
        })
        .collect();

    // Trim trailing underscores
    let name = name.trim_end_matches('_').to_string();

    // Handle empty result after trimming (shouldn't happen normally)
    if name.is_empty() {
        return "root".to_string();
    }

    // Prefix if it's a keyword or starts with a digit
    if RUST_KEYWORDS.contains(&name.as_str()) || name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("runme_{}", name)
    } else {
        name
    }
}

/// Given a list of relative paths to RUNME.rs files, compute a map from path to crate name.
///
/// Panics if two paths produce the same crate name (collision).
#[allow(dead_code)]
pub fn assign_crate_names<'a>(paths: &'a [&'a Path]) -> HashMap<&'a Path, String> {
    let mut result: HashMap<&'a Path, String> = HashMap::new();
    let mut seen: HashMap<String, &'a Path> = HashMap::new();

    for &path in paths {
        let name = crate_name_from_path(path);
        if let Some(&existing_path) = seen.get(&name) {
            panic!(
                "Crate name collision: both {:?} and {:?} produce crate name {:?}",
                existing_path, path, name
            );
        }
        seen.insert(name.clone(), path);
        result.insert(path, name);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_root_runme() {
        assert_eq!(crate_name_from_path(Path::new("RUNME.rs")), "root");
    }

    #[test]
    fn test_dot_slash_root_runme() {
        assert_eq!(crate_name_from_path(Path::new("./RUNME.rs")), "root");
    }

    #[test]
    fn test_nested_path() {
        assert_eq!(
            crate_name_from_path(Path::new("services/auth/RUNME.rs")),
            "services_auth"
        );
    }

    #[test]
    fn test_dashes_in_path() {
        assert_eq!(
            crate_name_from_path(Path::new("web-app/RUNME.rs")),
            "web_app"
        );
    }

    #[test]
    fn test_dots_in_path() {
        assert_eq!(
            crate_name_from_path(Path::new("my.service/RUNME.rs")),
            "my_service"
        );
    }

    #[test]
    fn test_deeply_nested() {
        assert_eq!(
            crate_name_from_path(Path::new("a/b/c/d/RUNME.rs")),
            "a_b_c_d"
        );
    }

    #[test]
    fn test_trailing_separators_trimmed() {
        // A path like "foo./RUNME.rs" produces "foo_" which should be trimmed to "foo"
        assert_eq!(
            crate_name_from_path(Path::new("foo./RUNME.rs")),
            "foo"
        );
    }

    #[test]
    fn test_keyword_prefixed() {
        assert_eq!(
            crate_name_from_path(Path::new("self/RUNME.rs")),
            "runme_self"
        );
    }

    #[test]
    fn test_keyword_mod() {
        assert_eq!(
            crate_name_from_path(Path::new("mod/RUNME.rs")),
            "runme_mod"
        );
    }

    #[test]
    fn test_keyword_type() {
        assert_eq!(
            crate_name_from_path(Path::new("type/RUNME.rs")),
            "runme_type"
        );
    }

    #[test]
    fn test_starts_with_digit() {
        assert_eq!(
            crate_name_from_path(Path::new("3rdparty/RUNME.rs")),
            "runme_3rdparty"
        );
    }

    #[test]
    fn test_digit_nested() {
        assert_eq!(
            crate_name_from_path(Path::new("9apps/web/RUNME.rs")),
            "runme_9apps_web"
        );
    }

    #[test]
    fn test_single_dir() {
        assert_eq!(
            crate_name_from_path(Path::new("web/RUNME.rs")),
            "web"
        );
    }

    #[test]
    fn test_mixed_separators() {
        assert_eq!(
            crate_name_from_path(Path::new("my-app.v2/sub/RUNME.rs")),
            "my_app_v2_sub"
        );
    }

    #[test]
    fn test_assign_crate_names_no_collision() {
        let paths: Vec<&Path> = vec![
            Path::new("RUNME.rs"),
            Path::new("services/auth/RUNME.rs"),
            Path::new("web-app/RUNME.rs"),
        ];
        let names = assign_crate_names(&paths);
        assert_eq!(names[Path::new("RUNME.rs")], "root");
        assert_eq!(names[Path::new("services/auth/RUNME.rs")], "services_auth");
        assert_eq!(names[Path::new("web-app/RUNME.rs")], "web_app");
    }

    #[test]
    #[should_panic(expected = "Crate name collision")]
    fn test_assign_crate_names_collision() {
        // Both produce "a_b"
        let paths: Vec<&Path> = vec![
            Path::new("a/b/RUNME.rs"),
            Path::new("a-b/RUNME.rs"),
        ];
        assign_crate_names(&paths);
    }

    #[test]
    fn test_unicode_in_path() {
        // Unicode chars are not replaced; they pass through as-is
        let name = crate_name_from_path(Path::new("café/RUNME.rs"));
        assert_eq!(name, "café");
    }

    #[test]
    fn test_unicode_deeply_nested() {
        let name = crate_name_from_path(Path::new("données/réseau/auth/RUNME.rs"));
        assert_eq!(name, "données_réseau_auth");
    }

    #[test]
    fn test_three_level_deep() {
        // Explicit 3-level depth
        assert_eq!(
            crate_name_from_path(Path::new("a/b/c/RUNME.rs")),
            "a_b_c"
        );
    }

    #[test]
    fn test_collision_dash_vs_slash() {
        // Verify that a/b and a-b indeed produce the same crate name
        assert_eq!(
            crate_name_from_path(Path::new("a/b/RUNME.rs")),
            crate_name_from_path(Path::new("a-b/RUNME.rs")),
        );
    }

    #[test]
    fn test_plain_runme_vs_dotslash_runme_same_name() {
        // "RUNME.rs" and "./RUNME.rs" must both produce "root"
        assert_eq!(
            crate_name_from_path(Path::new("RUNME.rs")),
            crate_name_from_path(Path::new("./RUNME.rs")),
        );
    }
}
