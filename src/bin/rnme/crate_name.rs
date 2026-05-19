use std::collections::HashMap;
use std::path::{Path, PathBuf};

use heck::ToSnakeCase;

/// Rust keywords that cannot be used as crate names.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "yield",
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

/// Compute the rename-resolved **basename** for a child RUNME.rs entry,
/// applying `[rnme.rename]` if present.
///
/// - `child_rel_path` — path to the child RUNME.rs relative to the
///   discovery root; its `parent()` supplies the original basename.
/// - `child_source_path` — the same file's path as it appears in the
///   discovery set; used solely as the lookup key into `renames`.
/// - `renames` — map from child source path to its raw rename value
///   (pre-normalization, as parsed from `[rnme.rename]`).
///
/// If the file is in the renames map, returns `heck::to_snake_case(value)`.
/// Otherwise returns the original parent-directory basename.
///
/// This is **only ever called for child entries**. The root is assigned
/// `"root"` directly by the caller; there is no `is_root` check.
pub fn resolved_basename(
    child_rel_path: &Path,
    child_source_path: &Path,
    renames: &HashMap<PathBuf, String>,
) -> String {
    let parent = child_rel_path.parent().unwrap_or(Path::new(""));
    let original_basename = parent
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    match renames.get(child_source_path) {
        Some(name) => name.to_snake_case(),
        None => original_basename,
    }
}

/// Compute the Rust crate/module identifier for a child node from its
/// rename-resolved effective directory path (slashes preserved).
///
/// `effective_dir` is the path from root to the child's directory after
/// rename substitution — e.g. `"services/auth_v2"`. This function flattens
/// it via the existing path-to-ident normalizer in `crate_name_from_path`
/// (slashes → underscores, keyword/digit guards), yielding `"services_auth_v2"`.
pub fn module_name_from_effective_dir(effective_dir: &Path) -> String {
    crate_name_from_path(&effective_dir.join("RUNME.rs"))
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
        assert_eq!(crate_name_from_path(Path::new("foo./RUNME.rs")), "foo");
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
        assert_eq!(crate_name_from_path(Path::new("mod/RUNME.rs")), "runme_mod");
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
        assert_eq!(crate_name_from_path(Path::new("web/RUNME.rs")), "web");
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
        let paths: Vec<&Path> = vec![Path::new("a/b/RUNME.rs"), Path::new("a-b/RUNME.rs")];
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
        assert_eq!(crate_name_from_path(Path::new("a/b/c/RUNME.rs")), "a_b_c");
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

    // -----------------------------------------------------------------------
    // `resolved_basename` and `module_name_from_effective_dir` tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolved_basename_no_rename() {
        let renames: HashMap<PathBuf, String> = HashMap::new();
        let out = resolved_basename(
            Path::new("foo/bar/RUNME.rs"),
            Path::new("/abs/foo/bar/RUNME.rs"),
            &renames,
        );
        assert_eq!(out, "bar");
    }

    #[test]
    fn test_resolved_basename_with_rename() {
        let mut renames: HashMap<PathBuf, String> = HashMap::new();
        renames.insert(PathBuf::from("/abs/foo/bar/RUNME.rs"), "baz".to_string());
        let out = resolved_basename(
            Path::new("foo/bar/RUNME.rs"),
            Path::new("/abs/foo/bar/RUNME.rs"),
            &renames,
        );
        assert_eq!(out, "baz");
    }

    #[test]
    fn test_resolved_basename_rename_snake_cases_hello_world() {
        let mut renames: HashMap<PathBuf, String> = HashMap::new();
        renames.insert(
            PathBuf::from("/abs/foo/RUNME.rs"),
            "Hello World".to_string(),
        );
        let out = resolved_basename(
            Path::new("foo/RUNME.rs"),
            Path::new("/abs/foo/RUNME.rs"),
            &renames,
        );
        assert_eq!(out, "hello_world");
    }

    #[test]
    fn test_resolved_basename_rename_camel_case() {
        let mut renames: HashMap<PathBuf, String> = HashMap::new();
        renames.insert(PathBuf::from("/abs/foo/RUNME.rs"), "FooBar".to_string());
        let out = resolved_basename(
            Path::new("foo/RUNME.rs"),
            Path::new("/abs/foo/RUNME.rs"),
            &renames,
        );
        assert_eq!(out, "foo_bar");
    }

    #[test]
    fn test_resolved_basename_rename_dashes() {
        let mut renames: HashMap<PathBuf, String> = HashMap::new();
        renames.insert(
            PathBuf::from("/abs/foo/RUNME.rs"),
            "foo-bar-v2".to_string(),
        );
        let out = resolved_basename(
            Path::new("foo/RUNME.rs"),
            Path::new("/abs/foo/RUNME.rs"),
            &renames,
        );
        assert_eq!(out, "foo_bar_v2");
    }

    #[test]
    fn test_resolved_basename_unmapped_path() {
        // Map has entries, but not for this child — falls back to original.
        let mut renames: HashMap<PathBuf, String> = HashMap::new();
        renames.insert(PathBuf::from("/abs/other/RUNME.rs"), "renamed".to_string());
        let out = resolved_basename(
            Path::new("foo/RUNME.rs"),
            Path::new("/abs/foo/RUNME.rs"),
            &renames,
        );
        assert_eq!(out, "foo");
    }

    #[test]
    fn test_module_name_from_effective_dir_single() {
        assert_eq!(
            module_name_from_effective_dir(Path::new("foo")),
            "foo"
        );
    }

    #[test]
    fn test_module_name_from_effective_dir_nested() {
        // Slashes flatten to underscores via crate_name_from_path
        assert_eq!(
            module_name_from_effective_dir(Path::new("services/auth_v2")),
            "services_auth_v2"
        );
    }
}
