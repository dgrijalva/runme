use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::crate_name::crate_name_from_path;
use crate::discover::DiscoveryResult;
use crate::frontmatter::{parse_frontmatter, rewrite_path_deps};
use crate::transform::transform_source;

/// Result of compiling a RUNME.rs file.
#[derive(Debug)]
pub struct CompileResult {
    /// Path to the compiled binary.
    pub binary_path: PathBuf,
}

/// Errors that can occur during compilation.
#[derive(Debug)]
pub enum CompileError {
    /// Failed to read the RUNME.rs source file.
    ReadSource(std::io::Error),
    /// Failed to create cache directory or write generated files.
    Io(std::io::Error),
    /// `cargo build` failed.
    CargoBuild(String),
    /// Could not determine the home directory for cache placement.
    NoHomeDir,
    /// Could not determine the runme library crate path.
    NoLibPath,
    /// Discovery result has no nearest RUNME.rs.
    NoRunmeFile,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::ReadSource(e) => write!(f, "failed to read source: {}", e),
            CompileError::Io(e) => write!(f, "I/O error: {}", e),
            CompileError::CargoBuild(msg) => write!(f, "cargo build failed: {}", msg),
            CompileError::NoHomeDir => write!(f, "could not determine home directory"),
            CompileError::NoLibPath => write!(f, "could not determine runme library path"),
            CompileError::NoRunmeFile => write!(f, "no RUNME.rs file in discovery result"),
        }
    }
}

impl std::error::Error for CompileError {}

const HASH_PREFIX_LEN: usize = 12;

/// Information about a single RUNME.rs file to be compiled into the workspace.
struct CrateEntry {
    /// Valid Rust crate name derived from the relative path.
    crate_name: String,
    /// Group key for this file (relative directory path, e.g. "services/auth").
    /// Root is "". Currently used only in tests but preserved as metadata.
    #[allow(dead_code)]
    group_key: String,
    /// Transformed source code (shebang stripped, __RUNME_GROUP injected, __runme_link appended).
    lib_source: String,
    /// Cargo.toml content for this lib crate.
    cargo_toml: String,
}

/// Compile a workspace from a discovery result, returning the path to the runner binary.
///
/// Always regenerates the workspace files, then runs `cargo build` (letting Cargo's
/// incremental compilation handle the rest). Cache directory is keyed by the absolute
/// path of the root RUNME.rs file, giving a stable `target/` directory.
pub fn compile_workspace(discovery: &DiscoveryResult) -> Result<CompileResult, CompileError> {
    let root_runme = discovery
        .nearest
        .as_ref()
        .ok_or(CompileError::NoRunmeFile)?;

    // Find the runme library crate path
    let runme_lib_path = find_runme_lib_path(root_runme)?;

    // Compute cache directory from hash of root RUNME.rs absolute path
    let root_abs = fs::canonicalize(root_runme).map_err(CompileError::Io)?;
    let cache_dir = cache_dir_for_root(&root_abs)?;

    // The root directory for computing relative paths
    let root_dir = root_runme
        .parent()
        .ok_or(CompileError::NoRunmeFile)?;

    // Collect all RUNME.rs files
    let mut all_files: Vec<PathBuf> = vec![root_runme.clone()];
    all_files.extend(discovery.children.iter().cloned());

    // Process each file into a CrateEntry
    let mut entries: Vec<CrateEntry> = Vec::new();
    for file in &all_files {
        let entry = process_runme_file(file, root_dir, &runme_lib_path)?;
        entries.push(entry);
    }

    // Generate the workspace
    generate_workspace(&cache_dir, &entries, &runme_lib_path)?;

    eprintln!("runme: compiling...");

    // Run cargo build
    let target_dir = cache_dir.join("target");
    let output = Command::new("cargo")
        .args(["build"])
        .current_dir(&cache_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .map_err(CompileError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CompileError::CargoBuild(stderr.to_string()));
    }

    let binary_path = target_dir.join("debug").join("runner");
    Ok(CompileResult { binary_path })
}

/// Process a single RUNME.rs file into a CrateEntry.
fn process_runme_file(
    file: &Path,
    root_dir: &Path,
    runme_lib_path: &Path,
) -> Result<CrateEntry, CompileError> {
    // Read source
    let source = fs::read_to_string(file).map_err(CompileError::ReadSource)?;

    // Compute relative path from root_dir
    let rel_path = file
        .strip_prefix(root_dir)
        .unwrap_or(file.as_ref());

    // Derive crate name
    let crate_name = crate_name_from_path(rel_path);

    // Compute group key: relative directory path, "" for root
    let group_key = rel_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    // Normalize: strip leading "./" and trailing "/"
    let group_key = group_key
        .strip_prefix("./")
        .unwrap_or(&group_key)
        .trim_end_matches('/')
        .to_string();
    // "." means root, normalize to ""
    let group_key = if group_key == "." { String::new() } else { group_key };

    // Transform source: strip shebang, inject __RUNME_GROUP, append __runme_link
    let lib_source = transform_source(&source, &group_key);

    // Parse frontmatter for dependencies
    let frontmatter = parse_frontmatter(&source);

    // Rewrite path deps relative to the original RUNME.rs directory
    let original_dir = file.parent().unwrap_or(Path::new("."));
    let rewritten_deps = rewrite_path_deps(&frontmatter.dependencies, original_dir);

    // Build Cargo.toml content
    let mut cargo_toml = format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
name = "{crate_name}"
path = "src/lib.rs"

[dependencies]
runme = {{ path = "{runme_lib}" }}
"#,
        crate_name = crate_name,
        runme_lib = runme_lib_path.display(),
    );

    for (name, version_spec) in &rewritten_deps {
        cargo_toml.push_str(&format!("{} = {}\n", name, version_spec));
    }

    Ok(CrateEntry {
        crate_name,
        group_key,
        lib_source,
        cargo_toml,
    })
}

/// Generate the full workspace structure on disk.
fn generate_workspace(
    cache_dir: &Path,
    entries: &[CrateEntry],
    runme_lib_path: &Path,
) -> Result<(), CompileError> {
    // Write each lib crate
    for entry in entries {
        let crate_dir = cache_dir.join(&entry.crate_name);
        let src_dir = crate_dir.join("src");
        fs::create_dir_all(&src_dir).map_err(CompileError::Io)?;
        fs::write(crate_dir.join("Cargo.toml"), &entry.cargo_toml).map_err(CompileError::Io)?;
        fs::write(src_dir.join("lib.rs"), &entry.lib_source).map_err(CompileError::Io)?;
    }

    // Generate runner crate
    let runner_dir = cache_dir.join("runner");
    let runner_src_dir = runner_dir.join("src");
    fs::create_dir_all(&runner_src_dir).map_err(CompileError::Io)?;

    // Runner Cargo.toml
    let mut runner_cargo = format!(
        r#"[package]
name = "runner"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "runner"
path = "src/main.rs"

[dependencies]
runme = {{ path = "{}" }}
"#,
        runme_lib_path.display(),
    );

    for entry in entries {
        runner_cargo.push_str(&format!(
            "{} = {{ path = \"../{}\" }}\n",
            entry.crate_name, entry.crate_name,
        ));
    }

    fs::write(runner_dir.join("Cargo.toml"), &runner_cargo).map_err(CompileError::Io)?;

    // Runner main.rs
    let runner_main = generate_runner_main(entries);
    fs::write(runner_src_dir.join("main.rs"), &runner_main).map_err(CompileError::Io)?;

    // Workspace Cargo.toml
    let mut members: Vec<String> = entries.iter().map(|e| format!("\"{}\"", e.crate_name)).collect();
    members.push("\"runner\"".to_string());
    let workspace_toml = format!(
        r#"[workspace]
members = [{}]
resolver = "3"
"#,
        members.join(", "),
    );

    fs::write(cache_dir.join("Cargo.toml"), &workspace_toml).map_err(CompileError::Io)?;

    Ok(())
}

/// Generate the runner crate's main.rs source.
///
/// The runner main:
/// 1. Calls __runme_link() on each lib crate to force linker inclusion of inventory registrations
/// 2. Runs init hooks (leaf-to-root, by counting `/` in group key)
/// 3. Builds a Registry from inventory
/// 4. Parses CLI args and dispatches
fn generate_runner_main(entries: &[CrateEntry]) -> String {
    let mut source = String::new();

    // __runme_link calls
    source.push_str("fn main() {\n");
    for entry in entries {
        source.push_str(&format!("    {}::__runme_link();\n", entry.crate_name));
    }
    source.push('\n');

    // Build tokio runtime and dispatch
    source.push_str(
        r#"    runme::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(async {
            // Run init hooks: collect from inventory, sort leaf-to-root
            // (deeper group keys first, by counting '/' separators)
            let mut inits: Vec<&runme::init::InitDef> = runme::inventory::iter::<runme::init::InitDef>.into_iter().collect();
            inits.sort_by(|a, b| {
                let depth_a = if a.group.is_empty() { 0 } else { a.group.matches('/').count() + 1 };
                let depth_b = if b.group.is_empty() { 0 } else { b.group.matches('/').count() + 1 };
                depth_b.cmp(&depth_a)
            });

            // Collect group display names (start with key, overridable by init)
            let mut group_names: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
            for group in runme::inventory::iter::<runme::init::GroupDef> {
                group_names.insert(group.key, group.key.to_string());
            }

            for init in &inits {
                let default_name = group_names.get(init.group).cloned().unwrap_or_else(|| init.group.to_string());
                let mut ctx = runme::init::InitContext::new(&default_name);
                (init.func)(&mut ctx);
                group_names.insert(init.group, ctx.group_name().to_string());
            }

            // Build registry from inventory
            let registry = runme::task::Registry::from_inventory();
            let args: Vec<String> = std::env::args().collect();

            if args.iter().any(|a| a == "--list") {
                for task in registry.list() {
                    let group_display = group_names.get(task.group).map(|s| s.as_str()).unwrap_or(task.group);
                    if group_display.is_empty() {
                        println!("{}: {}", task.name, task.description.unwrap_or(""));
                    } else {
                        println!("[{}] {}: {}", group_display, task.name, task.description.unwrap_or(""));
                    }
                }
                return;
            }

            if let Some(task_name) = args.get(1) {
                if let Err(e) = registry.run(task_name).await {
                    eprintln!("Error: {}", e);
                    std::process::exit(e.exit_code());
                }
            } else {
                println!("Available tasks:");
                for task in registry.list() {
                    let group_display = group_names.get(task.group).map(|s| s.as_str()).unwrap_or(task.group);
                    if group_display.is_empty() {
                        println!("  {}: {}", task.name, task.description.unwrap_or(""));
                    } else {
                        println!("  [{}] {}: {}", group_display, task.name, task.description.unwrap_or(""));
                    }
                }
            }
        });
}
"#,
    );

    source
}

/// Compute the cache directory for a root RUNME.rs file.
///
/// Hashes the absolute path to produce a stable, filesystem-safe directory name.
fn cache_dir_for_root(root_abs: &Path) -> Result<PathBuf, CompileError> {
    let home = home_dir().ok_or(CompileError::NoHomeDir)?;
    let mut hasher = Sha256::new();
    hasher.update(root_abs.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let hash_prefix = &hash[..HASH_PREFIX_LEN];
    Ok(home.join(".cache").join("runme").join(hash_prefix))
}

/// Find the absolute path to the `runme` library crate.
///
/// Strategy: look for `crates/runme` relative to the RUNME.rs file's directory,
/// walking up. This handles both the case where RUNME.rs is in the runme repo
/// itself and the general case where runme is installed.
fn find_runme_lib_path(runme_file: &Path) -> Result<PathBuf, CompileError> {
    // First, try to find it relative to the runme-cli binary's location.
    // When installed via `cargo install`, the source crate path won't work,
    // but during development the crates are in the repo.
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        // Walk up from the exe to find the workspace root
        let mut search = exe_dir.to_path_buf();
        loop {
            let candidate = search.join("crates").join("runme");
            if candidate.join("Cargo.toml").is_file() {
                return Ok(candidate);
            }
            if !search.pop() {
                break;
            }
        }
    }

    // Fallback: walk up from the RUNME.rs file location
    let start_dir = runme_file.parent().ok_or(CompileError::NoLibPath)?;
    let mut search = start_dir.to_path_buf();
    loop {
        let candidate = search.join("crates").join("runme");
        if candidate.join("Cargo.toml").is_file() {
            return Ok(candidate);
        }
        if !search.pop() {
            break;
        }
    }

    // Last resort: check if RUNME_LIB_PATH env var is set
    if let Ok(path) = std::env::var("RUNME_LIB_PATH") {
        let p = PathBuf::from(path);
        if p.join("Cargo.toml").is_file() {
            return Ok(p);
        }
    }

    Err(CompileError::NoLibPath)
}

/// Get the user's home directory.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_dir_for_root_deterministic() {
        let path = Path::new("/home/user/project/RUNME.rs");
        let dir1 = cache_dir_for_root(path).unwrap();
        let dir2 = cache_dir_for_root(path).unwrap();
        assert_eq!(dir1, dir2);
    }

    #[test]
    fn test_cache_dir_for_root_different_paths() {
        let dir1 = cache_dir_for_root(Path::new("/home/user/project/RUNME.rs")).unwrap();
        let dir2 = cache_dir_for_root(Path::new("/home/user/other/RUNME.rs")).unwrap();
        assert_ne!(dir1, dir2);
    }

    #[test]
    fn test_cache_dir_for_root_structure() {
        let dir = cache_dir_for_root(Path::new("/home/user/project/RUNME.rs")).unwrap();
        assert!(dir.to_string_lossy().contains(".cache/runme/"));
    }

    #[test]
    fn test_generate_runner_main_contains_link_calls() {
        let entries = vec![
            CrateEntry {
                crate_name: "root".to_string(),
                group_key: "".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
            },
            CrateEntry {
                crate_name: "services_auth".to_string(),
                group_key: "services/auth".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
            },
        ];
        let main_rs = generate_runner_main(&entries);
        assert!(main_rs.contains("root::__runme_link();"));
        assert!(main_rs.contains("services_auth::__runme_link();"));
        assert!(main_rs.contains("Registry::from_inventory()"));
        assert!(main_rs.contains("fn main()"));
    }

    #[test]
    fn test_generate_runner_main_init_ordering() {
        let entries = vec![
            CrateEntry {
                crate_name: "root".to_string(),
                group_key: "".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
            },
        ];
        let main_rs = generate_runner_main(&entries);
        // Verify that init sorting logic is present (leaf-to-root)
        assert!(main_rs.contains("depth_b.cmp(&depth_a)"));
    }

    #[test]
    fn test_process_runme_file() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create a minimal RUNME.rs
        let runme_path = tmp.path().join("RUNME.rs");
        fs::write(&runme_path, "#!/usr/bin/env runme\nfn hello() {}\n").unwrap();

        // Find runme lib path (needed for Cargo.toml generation)
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let runme_lib = workspace_root.join("crates").join("runme");
        if !runme_lib.join("Cargo.toml").is_file() {
            eprintln!("Skipping test: runme lib not found");
            return;
        }

        let entry = process_runme_file(&runme_path, tmp.path(), &runme_lib).unwrap();
        assert_eq!(entry.crate_name, "root");
        assert_eq!(entry.group_key, "");
        assert!(entry.lib_source.contains("const __RUNME_GROUP: &str = \"\";"));
        assert!(entry.lib_source.contains("pub fn __runme_link() {}"));
        assert!(!entry.lib_source.contains("#!/usr/bin/env runme"));
        assert!(entry.cargo_toml.contains("name = \"root\""));
    }

    #[test]
    fn test_process_runme_file_nested() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create a nested RUNME.rs
        let child_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&child_dir).unwrap();
        let runme_path = child_dir.join("RUNME.rs");
        fs::write(&runme_path, "fn migrate() {}\n").unwrap();

        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let runme_lib = workspace_root.join("crates").join("runme");
        if !runme_lib.join("Cargo.toml").is_file() {
            eprintln!("Skipping test: runme lib not found");
            return;
        }

        let entry = process_runme_file(&runme_path, tmp.path(), &runme_lib).unwrap();
        assert_eq!(entry.crate_name, "services_auth");
        assert_eq!(entry.group_key, "services/auth");
        assert!(entry.lib_source.contains("const __RUNME_GROUP: &str = \"services/auth\";"));
    }

    #[test]
    fn test_generate_workspace_structure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache_dir = tmp.path().join("workspace");
        fs::create_dir_all(&cache_dir).unwrap();

        let runme_lib = PathBuf::from("/fake/path/to/runme");

        let entries = vec![
            CrateEntry {
                crate_name: "root".to_string(),
                group_key: "".to_string(),
                lib_source: "// root lib".to_string(),
                cargo_toml: "[package]\nname = \"root\"\n".to_string(),
            },
            CrateEntry {
                crate_name: "services_auth".to_string(),
                group_key: "services/auth".to_string(),
                lib_source: "// auth lib".to_string(),
                cargo_toml: "[package]\nname = \"services_auth\"\n".to_string(),
            },
        ];

        generate_workspace(&cache_dir, &entries, &runme_lib).unwrap();

        // Verify workspace Cargo.toml
        let ws_toml = fs::read_to_string(cache_dir.join("Cargo.toml")).unwrap();
        assert!(ws_toml.contains("[workspace]"));
        assert!(ws_toml.contains("\"root\""));
        assert!(ws_toml.contains("\"services_auth\""));
        assert!(ws_toml.contains("\"runner\""));

        // Verify lib crate files
        assert!(cache_dir.join("root/src/lib.rs").exists());
        assert!(cache_dir.join("root/Cargo.toml").exists());
        assert!(cache_dir.join("services_auth/src/lib.rs").exists());
        assert!(cache_dir.join("services_auth/Cargo.toml").exists());

        // Verify runner crate
        assert!(cache_dir.join("runner/src/main.rs").exists());
        assert!(cache_dir.join("runner/Cargo.toml").exists());

        let runner_toml = fs::read_to_string(cache_dir.join("runner/Cargo.toml")).unwrap();
        assert!(runner_toml.contains("root = { path = \"../root\" }"));
        assert!(runner_toml.contains("services_auth = { path = \"../services_auth\" }"));

        let runner_main = fs::read_to_string(cache_dir.join("runner/src/main.rs")).unwrap();
        assert!(runner_main.contains("root::__runme_link();"));
        assert!(runner_main.contains("services_auth::__runme_link();"));
    }

    // -----------------------------------------------------------------------
    // Integration tests: full pipeline from RUNME.rs files → workspace
    // -----------------------------------------------------------------------

    /// Helper: resolve the real runme library path from this repo.
    /// Returns None if the library can't be found (skips the test).
    fn runme_lib_path() -> Option<PathBuf> {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let runme_lib = workspace_root.join("crates").join("runme");
        if runme_lib.join("Cargo.toml").is_file() {
            Some(runme_lib)
        } else {
            None
        }
    }

    /// Test 1: Single-file workspace generation.
    ///
    /// Creates a temp dir with one RUNME.rs, builds a DiscoveryResult,
    /// processes files, generates workspace, and verifies structure.
    #[test]
    fn test_integration_single_file_workspace() {
        let runme_lib = match runme_lib_path() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: runme lib not found");
                return;
            }
        };

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a single RUNME.rs at root
        let runme_path = tmp.path().join("RUNME.rs");
        fs::write(
            &runme_path,
            "#!/usr/bin/env runme\n\nfn hello() {}\n",
        )
        .unwrap();

        // Build DiscoveryResult with just this file
        let discovery = DiscoveryResult {
            nearest: Some(runme_path.clone()),
            children: vec![],
        };

        // Process files through the pipeline
        let root_dir = runme_path.parent().unwrap();
        let mut all_files: Vec<PathBuf> = vec![runme_path.clone()];
        all_files.extend(discovery.children.iter().cloned());

        let mut entries: Vec<CrateEntry> = Vec::new();
        for file in &all_files {
            let entry = process_runme_file(file, root_dir, &runme_lib).unwrap();
            entries.push(entry);
        }

        // Should have exactly 1 entry
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].crate_name, "root");
        assert_eq!(entries[0].group_key, "");

        // Generate workspace
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &runme_lib).unwrap();

        // Verify workspace Cargo.toml exists and lists "root" and "runner"
        let ws_toml = fs::read_to_string(cache_dir.join("Cargo.toml")).unwrap();
        assert!(ws_toml.contains("[workspace]"));
        assert!(ws_toml.contains("\"root\""));
        assert!(ws_toml.contains("\"runner\""));
        assert!(ws_toml.contains("resolver = \"3\""));

        // Verify one lib crate named "root"
        assert!(cache_dir.join("root").is_dir());
        assert!(cache_dir.join("root/Cargo.toml").is_file());
        assert!(cache_dir.join("root/src/lib.rs").is_file());

        // Verify lib.rs has __RUNME_GROUP injected with empty group for root
        let lib_rs = fs::read_to_string(cache_dir.join("root/src/lib.rs")).unwrap();
        assert!(
            lib_rs.contains("const __RUNME_GROUP: &str = \"\";"),
            "lib.rs should contain __RUNME_GROUP injection, got: {}",
            lib_rs,
        );
        assert!(lib_rs.contains("pub fn __runme_link() {}"));

        // Verify runner crate
        assert!(cache_dir.join("runner").is_dir());
        assert!(cache_dir.join("runner/Cargo.toml").is_file());
        assert!(cache_dir.join("runner/src/main.rs").is_file());

        // Verify runner Cargo.toml depends on "root"
        let runner_toml = fs::read_to_string(cache_dir.join("runner/Cargo.toml")).unwrap();
        assert!(
            runner_toml.contains("root = { path = \"../root\" }"),
            "runner Cargo.toml should depend on root, got: {}",
            runner_toml,
        );

        // Verify no other lib crates exist (only root + runner)
        let members: Vec<_> = fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(members.len(), 2, "Expected root + runner dirs, got: {:?}", members);
        assert!(members.contains(&"root".to_string()));
        assert!(members.contains(&"runner".to_string()));
    }

    /// Test 2: Multi-file workspace structure.
    ///
    /// Creates 3 RUNME.rs files (root, services/auth, web), processes them,
    /// generates workspace, and verifies 3 lib crates + runner with correct
    /// crate names and __RUNME_GROUP values.
    #[test]
    fn test_integration_multi_file_workspace() {
        let runme_lib = match runme_lib_path() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: runme lib not found");
                return;
            }
        };

        let tmp = tempfile::TempDir::new().unwrap();

        // Create root RUNME.rs
        let root_runme = tmp.path().join("RUNME.rs");
        fs::write(&root_runme, "#!/usr/bin/env runme\nfn deploy() {}\n").unwrap();

        // Create services/auth/RUNME.rs
        let auth_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&auth_dir).unwrap();
        let auth_runme = auth_dir.join("RUNME.rs");
        fs::write(&auth_runme, "fn migrate() {}\n").unwrap();

        // Create web/RUNME.rs
        let web_dir = tmp.path().join("web");
        fs::create_dir_all(&web_dir).unwrap();
        let web_runme = web_dir.join("RUNME.rs");
        fs::write(&web_runme, "fn build() {}\n").unwrap();

        // Build DiscoveryResult
        let discovery = DiscoveryResult {
            nearest: Some(root_runme.clone()),
            children: vec![auth_runme.clone(), web_runme.clone()],
        };

        // Process all files
        let root_dir = root_runme.parent().unwrap();
        let mut all_files: Vec<PathBuf> = vec![root_runme.clone()];
        all_files.extend(discovery.children.iter().cloned());

        let mut entries: Vec<CrateEntry> = Vec::new();
        for file in &all_files {
            let entry = process_runme_file(file, root_dir, &runme_lib).unwrap();
            entries.push(entry);
        }

        // Should have 3 entries
        assert_eq!(entries.len(), 3);

        // Verify crate names
        let crate_names: Vec<&str> = entries.iter().map(|e| e.crate_name.as_str()).collect();
        assert!(crate_names.contains(&"root"), "Expected 'root' in crate names: {:?}", crate_names);
        assert!(crate_names.contains(&"services_auth"), "Expected 'services_auth' in crate names: {:?}", crate_names);
        assert!(crate_names.contains(&"web"), "Expected 'web' in crate names: {:?}", crate_names);

        // Verify group keys
        let root_entry = entries.iter().find(|e| e.crate_name == "root").unwrap();
        assert_eq!(root_entry.group_key, "");
        let auth_entry = entries.iter().find(|e| e.crate_name == "services_auth").unwrap();
        assert_eq!(auth_entry.group_key, "services/auth");
        let web_entry = entries.iter().find(|e| e.crate_name == "web").unwrap();
        assert_eq!(web_entry.group_key, "web");

        // Generate workspace
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &runme_lib).unwrap();

        // Verify 3 lib crates + runner generated
        assert!(cache_dir.join("root/src/lib.rs").is_file());
        assert!(cache_dir.join("services_auth/src/lib.rs").is_file());
        assert!(cache_dir.join("web/src/lib.rs").is_file());
        assert!(cache_dir.join("runner/src/main.rs").is_file());

        // Verify each lib.rs has correct __RUNME_GROUP value
        let root_lib = fs::read_to_string(cache_dir.join("root/src/lib.rs")).unwrap();
        assert!(
            root_lib.contains("const __RUNME_GROUP: &str = \"\";"),
            "root lib.rs should have empty group, got: {}",
            root_lib,
        );

        let auth_lib = fs::read_to_string(cache_dir.join("services_auth/src/lib.rs")).unwrap();
        assert!(
            auth_lib.contains("const __RUNME_GROUP: &str = \"services/auth\";"),
            "auth lib.rs should have services/auth group, got: {}",
            auth_lib,
        );

        let web_lib = fs::read_to_string(cache_dir.join("web/src/lib.rs")).unwrap();
        assert!(
            web_lib.contains("const __RUNME_GROUP: &str = \"web\";"),
            "web lib.rs should have web group, got: {}",
            web_lib,
        );

        // Verify workspace Cargo.toml lists all members
        let ws_toml = fs::read_to_string(cache_dir.join("Cargo.toml")).unwrap();
        assert!(ws_toml.contains("\"root\""));
        assert!(ws_toml.contains("\"services_auth\""));
        assert!(ws_toml.contains("\"web\""));
        assert!(ws_toml.contains("\"runner\""));

        // Verify runner depends on all 3 lib crates
        let runner_toml = fs::read_to_string(cache_dir.join("runner/Cargo.toml")).unwrap();
        assert!(runner_toml.contains("root = { path = \"../root\" }"));
        assert!(runner_toml.contains("services_auth = { path = \"../services_auth\" }"));
        assert!(runner_toml.contains("web = { path = \"../web\" }"));
    }

    /// Test 3: Path dependency rewriting in generated workspace.
    ///
    /// Creates a RUNME.rs with frontmatter declaring a path dependency to a
    /// local crate, then verifies the generated Cargo.toml has the resolved
    /// absolute path.
    #[test]
    fn test_integration_path_dependency_rewriting() {
        let runme_lib = match runme_lib_path() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: runme lib not found");
                return;
            }
        };

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a local crate that the RUNME.rs will depend on
        let tools_dir = tmp.path().join("shared").join("tools");
        fs::create_dir_all(tools_dir.join("src")).unwrap();
        fs::write(
            tools_dir.join("Cargo.toml"),
            "[package]\nname = \"tools\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(tools_dir.join("src/lib.rs"), "pub fn helper() {}\n").unwrap();

        // Create a RUNME.rs with a path dependency referencing ../shared/tools
        let project_dir = tmp.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        let runme_path = project_dir.join("RUNME.rs");
        fs::write(
            &runme_path,
            r#"#!/usr/bin/env runme
//! [dependencies]
//! tools = { path = "../shared/tools" }

fn build() {}
"#,
        )
        .unwrap();

        // Process the file
        let root_dir = runme_path.parent().unwrap();
        let entry = process_runme_file(&runme_path, root_dir, &runme_lib).unwrap();

        // Verify the Cargo.toml has an absolute path for the tools dependency
        // The path should be resolved relative to the original RUNME.rs directory
        let expected_abs_prefix = tmp.path().to_string_lossy().to_string();
        assert!(
            entry.cargo_toml.contains("tools = "),
            "Cargo.toml should contain tools dependency, got: {}",
            entry.cargo_toml,
        );
        assert!(
            entry.cargo_toml.contains(&expected_abs_prefix),
            "Cargo.toml path dep should be resolved to absolute path under {}, got: {}",
            expected_abs_prefix,
            entry.cargo_toml,
        );

        // Generate workspace and verify the on-disk Cargo.toml
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        let entries = vec![entry];
        generate_workspace(&cache_dir, &entries, &runme_lib).unwrap();

        let lib_toml = fs::read_to_string(cache_dir.join("root/Cargo.toml")).unwrap();
        assert!(
            lib_toml.contains(&expected_abs_prefix),
            "Generated Cargo.toml should have absolute path, got: {}",
            lib_toml,
        );
        // Verify it does NOT contain the relative path "../shared/tools" literally
        assert!(
            !lib_toml.contains("\"../shared/tools\""),
            "Generated Cargo.toml should NOT contain relative path, got: {}",
            lib_toml,
        );
    }

    /// Test 4: Runner main.rs content for a multi-file tree.
    ///
    /// Generates a workspace for a multi-file tree and verifies the runner's
    /// main.rs contains __runme_link() calls for each crate, init hook
    /// collection/sorting logic, and registry building.
    #[test]
    fn test_integration_runner_main_content() {
        let runme_lib = match runme_lib_path() {
            Some(p) => p,
            None => {
                eprintln!("Skipping test: runme lib not found");
                return;
            }
        };

        let tmp = tempfile::TempDir::new().unwrap();

        // Create 3 RUNME.rs files
        let root_runme = tmp.path().join("RUNME.rs");
        fs::write(&root_runme, "fn deploy() {}\n").unwrap();

        let svc_dir = tmp.path().join("services").join("api");
        fs::create_dir_all(&svc_dir).unwrap();
        let svc_runme = svc_dir.join("RUNME.rs");
        fs::write(&svc_runme, "fn serve() {}\n").unwrap();

        let infra_dir = tmp.path().join("infra");
        fs::create_dir_all(&infra_dir).unwrap();
        let infra_runme = infra_dir.join("RUNME.rs");
        fs::write(&infra_runme, "fn provision() {}\n").unwrap();

        // Process files
        let root_dir = root_runme.parent().unwrap();
        let all_files = vec![root_runme.clone(), svc_runme.clone(), infra_runme.clone()];

        let mut entries: Vec<CrateEntry> = Vec::new();
        for file in &all_files {
            let entry = process_runme_file(file, root_dir, &runme_lib).unwrap();
            entries.push(entry);
        }

        // Generate workspace
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &runme_lib).unwrap();

        // Read the generated runner main.rs
        let runner_main = fs::read_to_string(cache_dir.join("runner/src/main.rs")).unwrap();

        // Verify __runme_link() calls for each crate
        assert!(
            runner_main.contains("root::__runme_link();"),
            "runner main should call root::__runme_link(), got:\n{}",
            runner_main,
        );
        assert!(
            runner_main.contains("services_api::__runme_link();"),
            "runner main should call services_api::__runme_link(), got:\n{}",
            runner_main,
        );
        assert!(
            runner_main.contains("infra::__runme_link();"),
            "runner main should call infra::__runme_link(), got:\n{}",
            runner_main,
        );

        // Verify init hook collection from inventory
        assert!(
            runner_main.contains("runme::inventory::iter::<runme::init::InitDef>"),
            "runner main should collect InitDefs from inventory, got:\n{}",
            runner_main,
        );

        // Verify init sorting logic (leaf-to-root: deeper groups first)
        assert!(
            runner_main.contains("depth_b.cmp(&depth_a)"),
            "runner main should sort inits leaf-to-root, got:\n{}",
            runner_main,
        );

        // Verify group name collection from inventory
        assert!(
            runner_main.contains("runme::inventory::iter::<runme::init::GroupDef>"),
            "runner main should collect GroupDefs from inventory, got:\n{}",
            runner_main,
        );

        // Verify registry building
        assert!(
            runner_main.contains("runme::task::Registry::from_inventory()"),
            "runner main should build Registry from inventory, got:\n{}",
            runner_main,
        );

        // Verify the runner has a fn main() entry point
        assert!(
            runner_main.contains("fn main()"),
            "runner main should have fn main(), got:\n{}",
            runner_main,
        );

        // Verify the tokio runtime is built
        assert!(
            runner_main.contains("runme::tokio::runtime::Builder::new_multi_thread()"),
            "runner main should build tokio runtime, got:\n{}",
            runner_main,
        );

        // Verify --list flag handling is present
        assert!(
            runner_main.contains("--list"),
            "runner main should handle --list flag, got:\n{}",
            runner_main,
        );
    }
}
