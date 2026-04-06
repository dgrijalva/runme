use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::frontmatter::{parse_frontmatter, strip_shebang};

/// Result of compiling a RUNME.rs file.
#[derive(Debug)]
pub struct CompileResult {
    /// Path to the compiled binary.
    pub binary_path: PathBuf,
    /// Whether the binary was served from cache (true) or freshly compiled (false).
    pub was_cached: bool,
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
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::ReadSource(e) => write!(f, "failed to read source: {}", e),
            CompileError::Io(e) => write!(f, "I/O error: {}", e),
            CompileError::CargoBuild(msg) => write!(f, "cargo build failed: {}", msg),
            CompileError::NoHomeDir => write!(f, "could not determine home directory"),
            CompileError::NoLibPath => write!(f, "could not determine runme library path"),
        }
    }
}

impl std::error::Error for CompileError {}

const HASH_PREFIX_LEN: usize = 8;
const HASH_MARKER_FILE: &str = ".runme-hash";

/// Compile a RUNME.rs file, returning the path to the resulting binary.
///
/// Uses content-hash caching to skip recompilation when source hasn't changed.
/// Cache directory: `~/.cache/runme/<first-8-chars-of-hash>/`
pub fn compile(runme_file: &Path) -> Result<CompileResult, CompileError> {
    // Read source
    let source = fs::read_to_string(runme_file).map_err(CompileError::ReadSource)?;

    // Compute content hash
    let hash = content_hash(&source);
    let hash_prefix = &hash[..HASH_PREFIX_LEN];

    // Determine cache directory
    let cache_dir = cache_dir_for(hash_prefix)?;

    // Check if cached binary exists and hash matches
    let binary_path = binary_path_in(&cache_dir);
    if is_cached(&cache_dir, &hash) && binary_path.exists() {
        return Ok(CompileResult {
            binary_path,
            was_cached: true,
        });
    }

    // Need to compile: generate the Cargo project
    generate_project(&cache_dir, &source, runme_file)?;

    // Run cargo build.
    // Set CARGO_TARGET_DIR explicitly so that cargo doesn't follow the path
    // dependency back into the workspace and use its target directory instead.
    let target_dir = cache_dir.join("target");
    let output = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&cache_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .map_err(|e| CompileError::Io(e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CompileError::CargoBuild(stderr.to_string()));
    }

    // Write hash marker for future cache checks
    let marker_path = cache_dir.join(HASH_MARKER_FILE);
    fs::write(&marker_path, &hash).map_err(CompileError::Io)?;

    Ok(CompileResult {
        binary_path,
        was_cached: false,
    })
}

/// Compute the SHA-256 hash of the source content and return it as a hex string.
fn content_hash(source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Get the cache directory for a given hash prefix.
fn cache_dir_for(hash_prefix: &str) -> Result<PathBuf, CompileError> {
    let home = home_dir().ok_or(CompileError::NoHomeDir)?;
    Ok(home.join(".cache").join("runme").join(hash_prefix))
}

/// Get the expected binary path inside a cache directory.
fn binary_path_in(cache_dir: &Path) -> PathBuf {
    cache_dir
        .join("target")
        .join("release")
        .join("runme-script")
}

/// Check if the cache is valid (marker file exists and matches the full hash).
fn is_cached(cache_dir: &Path, expected_hash: &str) -> bool {
    let marker_path = cache_dir.join(HASH_MARKER_FILE);
    match fs::read_to_string(&marker_path) {
        Ok(stored_hash) => stored_hash == expected_hash,
        Err(_) => false,
    }
}

/// Generate a Cargo project in the cache directory from the RUNME.rs source.
fn generate_project(
    cache_dir: &Path,
    source: &str,
    runme_file: &Path,
) -> Result<(), CompileError> {
    // Determine the absolute path to the runme library crate
    let runme_lib_path = find_runme_lib_path(runme_file)?;

    // Create directories
    let src_dir = cache_dir.join("src");
    fs::create_dir_all(&src_dir).map_err(CompileError::Io)?;

    // Parse frontmatter for extra dependencies
    let frontmatter = parse_frontmatter(source);

    // Generate Cargo.toml
    let mut cargo_toml = format!(
        r#"[package]
name = "runme-script"
version = "0.1.0"
edition = "2024"

[dependencies]
runme = {{ path = "{}" }}
"#,
        runme_lib_path.display()
    );

    for (name, version_spec) in &frontmatter.dependencies {
        cargo_toml.push_str(&format!("{} = {}\n", name, version_spec));
    }

    fs::write(cache_dir.join("Cargo.toml"), &cargo_toml).map_err(CompileError::Io)?;

    // Write the source as src/main.rs with shebang stripped
    let stripped_source = strip_shebang(source);
    fs::write(src_dir.join("main.rs"), stripped_source).map_err(CompileError::Io)?;

    Ok(())
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
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
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
    }

    // Fallback: walk up from the RUNME.rs file location
    let start_dir = runme_file
        .parent()
        .ok_or(CompileError::NoLibPath)?;
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
    fn test_content_hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn test_content_hash_different_for_different_input() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_cache_dir_structure() {
        let dir = cache_dir_for("abcd1234").unwrap();
        assert!(dir.to_string_lossy().contains(".cache/runme/abcd1234"));
    }

    #[test]
    fn test_is_cached_false_when_no_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!is_cached(tmp.path(), "somehash"));
    }

    #[test]
    fn test_is_cached_true_when_marker_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(HASH_MARKER_FILE);
        fs::write(&marker, "myhash123").unwrap();
        assert!(is_cached(tmp.path(), "myhash123"));
    }

    #[test]
    fn test_is_cached_false_when_marker_mismatches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker = tmp.path().join(HASH_MARKER_FILE);
        fs::write(&marker, "oldhash").unwrap();
        assert!(!is_cached(tmp.path(), "newhash"));
    }

    #[test]
    fn test_compile_and_cache() {
        // This test requires the runme workspace to be present on disk.
        // It creates a simple RUNME.rs, compiles it, and verifies caching.
        let tmp = tempfile::TempDir::new().unwrap();

        // We need to find the runme lib path for the generated Cargo.toml.
        // Since we're running in the workspace, find it from current dir.
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();

        let runme_lib = workspace_root.join("crates").join("runme");
        if !runme_lib.join("Cargo.toml").is_file() {
            // Skip test if we can't find the runme lib (CI or unusual layout)
            eprintln!("Skipping test_compile_and_cache: runme lib not found at {:?}", runme_lib);
            return;
        }

        // The RUNME.rs must match the current TaskFn signature.
        // Phase 3 changed TaskDef.func to an async function pointer, so we
        // use the raw inventory API rather than relying on the #[task] macro
        // (which may not yet wrap sync fns into async).
        let source = format!(
            r#"use runme::prelude::*;
use std::pin::Pin;
use std::future::Future;

fn hello(ctx: &TaskContext) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {{
    Box::pin(async move {{
        println!("Hello from compile test: {{}}", ctx.name);
    }})
}}

runme::inventory::submit! {{
    TaskDef {{
        name: "hello",
        description: Some("Test task"),
        watch: None,
        depends_on: &[],
        func: hello,
    }}
}}

fn main() {{
    let registry = runme::task::Registry::from_inventory();
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list") {{
        for task in registry.list() {{
            println!("{{}}: {{}}", task.name, task.description.unwrap_or(""));
        }}
        return;
    }}

    let rt = runme::tokio::runtime::Runtime::new().unwrap();
    if let Some(task_name) = args.get(1) {{
        rt.block_on(registry.run(task_name));
    }} else {{
        println!("Available tasks:");
        for task in registry.list() {{
            println!("  {{}}: {{}}", task.name, task.description.unwrap_or(""));
        }}
    }}
}}
"#
        );

        let runme_path = tmp.path().join("RUNME.rs");
        fs::write(&runme_path, &source).unwrap();

        // Clean up any stale cache from previous test runs
        let hash = content_hash(&source);
        let cache_dir = cache_dir_for(&hash[..HASH_PREFIX_LEN]).unwrap();
        let _ = fs::remove_dir_all(&cache_dir);

        // Set RUNME_LIB_PATH so compile can find the library
        // SAFETY: This test is not run in parallel with other tests that depend
        // on environment variables.
        unsafe { std::env::set_var("RUNME_LIB_PATH", &runme_lib) };

        // First compile
        let result1 = compile(&runme_path).expect("first compile should succeed");
        assert!(!result1.was_cached, "first compile should not be cached");
        assert!(result1.binary_path.exists(), "binary should exist after compile at {:?}", result1.binary_path);

        // Second compile -- should be cached
        let result2 = compile(&runme_path).expect("second compile should succeed");
        assert!(result2.was_cached, "second compile should be cached");

        // Verify the compiled binary actually works by running it
        let output = std::process::Command::new(&result1.binary_path)
            .arg("--list")
            .output()
            .expect("compiled binary should be executable");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello:"),
            "expected task listing, got: {}",
            stdout
        );

        // Verify running the task produces output
        let output = std::process::Command::new(&result1.binary_path)
            .arg("hello")
            .output()
            .expect("compiled binary should run task");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Hello from compile test: hello"),
            "expected task output, got: {}",
            stdout
        );

        // Clean up the cache dir
        let hash = content_hash(&source);
        let cache_dir = cache_dir_for(&hash[..HASH_PREFIX_LEN]).unwrap();
        let _ = fs::remove_dir_all(&cache_dir);

        // SAFETY: Same as above.
        unsafe { std::env::remove_var("RUNME_LIB_PATH") };
    }
}
