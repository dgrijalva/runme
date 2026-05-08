//! RUNME.rs file discovery shared between the outer driver and the
//! supervisor.
//!
//! The outer driver uses [`discover`] to locate the nearest RUNME.rs from
//! a starting directory and any descendant RUNME.rs files (workspace
//! members). The MCP supervisor uses the same logic so its file-watcher
//! sees the exact same set of files the compile pipeline will.
//!
//! This module performs no I/O beyond filesystem reads: walking up looks
//! at directory ancestors, and walking down delegates to the `ignore`
//! crate so `.gitignore`, `.git/info/exclude`, and global ignore rules
//! are respected.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// Standard filename rnme looks for to identify a tasks file.
pub const RUNME_FILENAME: &str = "RUNME.rs";

/// Result of RUNME.rs file discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryResult {
    /// The nearest RUNME.rs found (walking up from the starting directory).
    pub nearest: Option<PathBuf>,
    /// All RUNME.rs files found in the subtree rooted at the nearest RUNME.rs
    /// (walking down), excluding the nearest itself.
    pub children: Vec<PathBuf>,
}

impl DiscoveryResult {
    /// Iterator over every discovered RUNME.rs (nearest + children).
    pub fn all_files(&self) -> impl Iterator<Item = &PathBuf> {
        self.nearest.iter().chain(self.children.iter())
    }
}

/// Find RUNME.rs files relative to the given directory.
///
/// First walks UP from `from` to locate the nearest RUNME.rs, then walks DOWN
/// from that directory to find child RUNME.rs files. The downward walk uses the
/// `ignore` crate so `.gitignore` rules are respected automatically.
pub fn discover(from: &Path) -> DiscoveryResult {
    let nearest = walk_up(from);

    let children = match &nearest {
        Some(nearest_path) => {
            let root_dir = nearest_path
                .parent()
                .expect("nearest RUNME.rs should have a parent directory");
            walk_down(root_dir, nearest_path)
        }
        None => Vec::new(),
    };

    DiscoveryResult { nearest, children }
}

/// Walk up from `from` checking each directory for RUNME.rs.
fn walk_up(from: &Path) -> Option<PathBuf> {
    let mut current = if from.is_file() {
        from.parent()?.to_path_buf()
    } else {
        from.to_path_buf()
    };

    loop {
        let candidate = current.join(RUNME_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Walk down from `root_dir` to find child RUNME.rs files, excluding `exclude`.
fn walk_down(root_dir: &Path, exclude: &Path) -> Vec<PathBuf> {
    let mut children = Vec::new();

    let walker = WalkBuilder::new(root_dir)
        .hidden(true) // skip hidden files/dirs
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .map(|n| n == RUNME_FILENAME)
                .unwrap_or(false)
            && path != exclude
        {
            children.push(path.to_path_buf());
        }
    }

    children
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_walk_up_finds_runme_in_current_dir() {
        let tmp = TempDir::new().unwrap();
        let runme_path = tmp.path().join(RUNME_FILENAME);
        fs::write(&runme_path, "// test").unwrap();

        let result = discover(tmp.path());
        assert_eq!(result.nearest.as_deref(), Some(runme_path.as_path()));
    }

    #[test]
    fn test_walk_up_finds_runme_in_parent() {
        let tmp = TempDir::new().unwrap();
        let runme_path = tmp.path().join(RUNME_FILENAME);
        fs::write(&runme_path, "// test").unwrap();

        let subdir = tmp.path().join("child").join("grandchild");
        fs::create_dir_all(&subdir).unwrap();

        let result = discover(&subdir);
        assert_eq!(result.nearest.as_deref(), Some(runme_path.as_path()));
    }

    #[test]
    fn test_walk_up_returns_none_when_no_runme() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("empty");
        fs::create_dir_all(&subdir).unwrap();

        let result = discover(&subdir);
        assert!(result.nearest.is_none());
        assert!(result.children.is_empty());
    }

    #[test]
    fn test_walk_down_finds_children() {
        let tmp = TempDir::new().unwrap();
        let root_runme = tmp.path().join(RUNME_FILENAME);
        fs::write(&root_runme, "// root").unwrap();

        let child_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&child_dir).unwrap();
        let child_runme = child_dir.join(RUNME_FILENAME);
        fs::write(&child_runme, "// auth").unwrap();

        let child_dir2 = tmp.path().join("web");
        fs::create_dir_all(&child_dir2).unwrap();
        let child_runme2 = child_dir2.join(RUNME_FILENAME);
        fs::write(&child_runme2, "// web").unwrap();

        let result = discover(tmp.path());
        assert_eq!(result.nearest.as_deref(), Some(root_runme.as_path()));
        assert_eq!(result.children.len(), 2);
        assert!(result.children.contains(&child_runme));
        assert!(result.children.contains(&child_runme2));
    }

    #[test]
    fn test_walk_down_respects_gitignore() {
        let tmp = TempDir::new().unwrap();
        let root_runme = tmp.path().join(RUNME_FILENAME);
        fs::write(&root_runme, "// root").unwrap();

        // The ignore crate requires a .git directory to recognize .gitignore files
        fs::create_dir_all(tmp.path().join(".git")).unwrap();

        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();

        let ignored_dir = tmp.path().join("target").join("debug");
        fs::create_dir_all(&ignored_dir).unwrap();
        fs::write(ignored_dir.join(RUNME_FILENAME), "// should be ignored").unwrap();

        let normal_dir = tmp.path().join("src");
        fs::create_dir_all(&normal_dir).unwrap();
        let normal_runme = normal_dir.join(RUNME_FILENAME);
        fs::write(&normal_runme, "// normal").unwrap();

        let result = discover(tmp.path());
        assert_eq!(result.children.len(), 1);
        assert!(result.children.contains(&normal_runme));
    }

    #[test]
    fn test_discover_from_subdirectory_finds_parent_and_children() {
        let tmp = TempDir::new().unwrap();
        let root_runme = tmp.path().join(RUNME_FILENAME);
        fs::write(&root_runme, "// root").unwrap();

        let child_dir = tmp.path().join("sub");
        fs::create_dir_all(&child_dir).unwrap();
        let child_runme = child_dir.join(RUNME_FILENAME);
        fs::write(&child_runme, "// sub").unwrap();

        let deep_dir = tmp.path().join("sub").join("deep");
        fs::create_dir_all(&deep_dir).unwrap();

        let result = discover(&deep_dir);
        assert_eq!(result.nearest.as_deref(), Some(child_runme.as_path()));
    }
}
