use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use std::collections::{BTreeMap, HashMap};

use sha2::{Digest, Sha256};

use crate::codegen::{CrateEntry, generate_runner_main};
use crate::crate_name::{crate_name_from_path, module_name_from_effective_dir, resolved_basename};
use crate::frontmatter::{Frontmatter, parse_frontmatter, rewrite_path_deps};
use crate::transform::transform_source;
use rnme::discover::DiscoveryResult;

/// Result of compiling a RUNME.rs file.
#[derive(Debug)]
pub struct CompileResult {
    /// Path to the compiled binary.
    pub binary_path: PathBuf,
    /// Path to the generated workspace cache directory.
    pub cache_dir: PathBuf,
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
    /// Could not determine the rnme library crate path.
    NoLibPath,
    /// Discovery result has no nearest RUNME.rs.
    NoRunmeFile,
    /// Two sibling RUNME.rs files normalize to the same module name.
    SiblingNameCollision {
        path_a: PathBuf,
        path_b: PathBuf,
        resolved_name: String,
        suggestion: String,
    },
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::ReadSource(e) => write!(f, "failed to read source: {}", e),
            CompileError::Io(e) => write!(f, "I/O error: {}", e),
            CompileError::CargoBuild(msg) => write!(f, "cargo build failed: {}", msg),
            CompileError::NoHomeDir => write!(f, "could not determine home directory"),
            CompileError::NoLibPath => write!(f, "could not determine rnme library path"),
            CompileError::NoRunmeFile => write!(f, "no RUNME.rs file in discovery result"),
            CompileError::SiblingNameCollision { path_a, path_b, resolved_name, suggestion } => {
                write!(
                    f,
                    "sibling RUNME.rs files collide on module name `{resolved_name}`:\n  \
                     (a) {path_a}\n  \
                     (b) {path_b}\n\
                     Add `[rnme.rename]` frontmatter to one of them to resolve the collision.\n\
                     Suggested rename for (b):\n\
                     \n\
                     //! [rnme.rename]\n\
                     //! name = \"{suggestion}\"\n",
                    path_a = path_a.display(),
                    path_b = path_b.display(),
                )
            }
        }
    }
}

impl std::error::Error for CompileError {}

const HASH_PREFIX_LEN: usize = 12;

/// Compile a workspace from a discovery result, returning the path to the runner binary.
///
/// Always regenerates the workspace files, then runs `cargo build` (letting Cargo's
/// incremental compilation handle the rest). Cache directory is keyed by the absolute
/// path of the root RUNME.rs file, giving a stable `target/` directory.
pub fn compile_workspace(discovery: &DiscoveryResult) -> Result<CompileResult, CompileError> {
    let root_rnme = discovery
        .nearest
        .as_ref()
        .ok_or(CompileError::NoRunmeFile)?;

    // Find the rnme library crate path
    let rnme_lib_path = find_rnme_lib_path(root_rnme)?;

    // Compute cache directory from hash of root RUNME.rs absolute path
    let root_abs = fs::canonicalize(root_rnme).map_err(CompileError::Io)?;
    let cache_dir = cache_dir_for_root(&root_abs)?;

    // The root directory for computing relative paths
    let root_dir = root_rnme.parent().ok_or(CompileError::NoRunmeFile)?;

    // Build the module tree (root + recursive children). Rename application
    // happens inside the child-iteration loop in `build_module_tree`, so the
    // root's `[rnme.rename]` (if any) is never consulted.
    let tree = build_module_tree(discovery, root_dir)?;

    // Flatten depth-first into the existing CrateEntry shape that the
    // generators consume. Collision detection fires here — before cargo build.
    let mut entries: Vec<CrateEntry> = Vec::new();
    flatten_tree_into_entries(&tree, &rnme_lib_path, &mut entries)?;

    // Generate the workspace
    generate_workspace(&cache_dir, &entries, &rnme_lib_path)?;

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
    Ok(CompileResult {
        binary_path,
        cache_dir,
    })
}

/// One node in the discovered RUNME.rs module tree.
///
/// Root is constructed outside any iteration loop with `module_name = "root"`
/// and `effective_dir = ""`. Children are constructed inside the recursive
/// child-iteration loop in `build_module_tree`, where their `module_name`
/// comes from `normalize_module_name(child_path, &renames)`.
///
/// Source and frontmatter are stored on the node so workspace generation
/// can build a `CrateEntry` without re-reading the file.
#[derive(Debug)]
struct ModuleNode {
    /// The RUNME.rs file backing this node (as discovered).
    path: PathBuf,
    /// Already-resolved module/crate name. Root: `"root"`. Children: result
    /// of `normalize_module_name`.
    module_name: String,
    /// Effective relative directory from `root_dir`, with renames applied.
    /// Root: `""`. Children: `parent.effective_dir.join(module_name)`.
    effective_dir: PathBuf,
    /// Parsed frontmatter; root's `rename` is intentionally never consulted.
    frontmatter: Frontmatter,
    /// Source text for `transform_source`.
    source: String,
    /// Recursive child nodes.
    children: Vec<ModuleNode>,
}

/// Build the discovered RUNME.rs module tree.
///
/// Root is created outside any loop with `module_name = "root"`; its
/// `frontmatter.rename` (if any) is structurally inaccessible to the rest
/// of the pipeline because no code path treats root as a child.
///
/// Children are visited recursively inside the child-iteration loop, where
/// `normalize_module_name` applies the rename map.
fn build_module_tree(
    discovery: &DiscoveryResult,
    root_dir: &Path,
) -> Result<ModuleNode, CompileError> {
    let root_rnme = discovery
        .nearest
        .as_ref()
        .ok_or(CompileError::NoRunmeFile)?;

    // Build the rename map by walking all child entries and pulling out their
    // `[rnme.rename]` values. Root is never in this map.
    let mut renames: HashMap<PathBuf, String> = HashMap::new();
    for child in &discovery.children {
        let src = fs::read_to_string(child).map_err(CompileError::ReadSource)?;
        let fm = parse_frontmatter(&src);
        if let Some(name) = fm.rename {
            renames.insert(child.clone(), name);
        }
    }

    // Read root once. Its frontmatter is parsed but the `rename` field is
    // intentionally not consulted by `module_tree_root` or any descendant
    // construction.
    let root_source = fs::read_to_string(root_rnme).map_err(CompileError::ReadSource)?;
    let root_frontmatter = parse_frontmatter(&root_source);

    let root_children: Vec<&PathBuf> = discovery.children.iter().collect();
    let children = collect_children_of(root_rnme, &root_children, root_dir, Path::new(""), &renames)?;

    Ok(ModuleNode {
        path: root_rnme.clone(),
        module_name: "root".to_string(),
        effective_dir: PathBuf::new(),
        frontmatter: root_frontmatter,
        source: root_source,
        children,
    })
}

/// Recursively collect direct children of `parent_rnme`.
///
/// A "direct child" is a RUNME.rs whose parent directory's deepest RUNME.rs
/// ancestor (among the candidates) is `parent_rnme`. Each direct child is
/// constructed, its name resolved via `normalize_module_name`, and its own
/// children collected by recursing on the remaining candidate set.
fn collect_children_of(
    parent_rnme: &Path,
    candidates: &[&PathBuf],
    root_dir: &Path,
    parent_effective_dir: &Path,
    renames: &HashMap<PathBuf, String>,
) -> Result<Vec<ModuleNode>, CompileError> {
    let parent_dir = parent_rnme.parent().unwrap_or(Path::new(""));
    let mut out: Vec<ModuleNode> = Vec::new();

    for candidate in candidates {
        // Determine the deepest RUNME.rs that is a strict ancestor of `candidate`
        // among `candidates` plus `parent_rnme`.
        let cand_parent = candidate.parent().unwrap_or(Path::new(""));
        if !is_strict_descendant(cand_parent, parent_dir) {
            continue;
        }
        // Skip if any other candidate sits between `parent_rnme` and `candidate`.
        let mut has_closer_ancestor = false;
        for other in candidates {
            if other.as_path() == candidate.as_path() {
                continue;
            }
            let other_dir = other.parent().unwrap_or(Path::new(""));
            if is_strict_descendant(cand_parent, other_dir)
                && is_strict_descendant(other_dir, parent_dir)
            {
                has_closer_ancestor = true;
                break;
            }
        }
        if has_closer_ancestor {
            continue;
        }

        // Construct this child node.
        let rel_path = candidate
            .strip_prefix(root_dir)
            .unwrap_or(candidate.as_path());

        // Resolve the *basename* (rename applied or original) for the
        // immediate directory containing this child's RUNME.rs.
        let basename = resolved_basename(rel_path, candidate, renames);

        // Compute the path segment from `parent_dir` to `cand_parent`
        // (inclusive of `cand_parent`'s basename), then swap that
        // basename for the resolved one. The result is the *relative
        // path from the parent's effective dir to this child's
        // effective dir*, preserving any intermediate structural dirs
        // (dirs without a RUNME.rs) along the way.
        let segment = cand_parent
            .strip_prefix(parent_dir)
            .unwrap_or(cand_parent);
        let segment_with_swap = segment
            .parent()
            .unwrap_or(Path::new(""))
            .join(&basename);

        let effective_dir = parent_effective_dir.join(&segment_with_swap);
        let module_name = module_name_from_effective_dir(&effective_dir);

        let source = fs::read_to_string(candidate).map_err(CompileError::ReadSource)?;
        let frontmatter = parse_frontmatter(&source);

        // Recurse: gather candidates for which this child is a strict ancestor.
        let descendants: Vec<&PathBuf> = candidates
            .iter()
            .copied()
            .filter(|c| c.as_path() != candidate.as_path())
            .filter(|c| {
                let cdir = c.parent().unwrap_or(Path::new(""));
                is_strict_descendant(cdir, cand_parent)
            })
            .collect();

        let grandchildren =
            collect_children_of(candidate, &descendants, root_dir, &effective_dir, renames)?;

        out.push(ModuleNode {
            path: candidate.to_path_buf(),
            module_name,
            effective_dir,
            frontmatter,
            source,
            children: grandchildren,
        });
    }

    Ok(out)
}

/// True iff `desc` is a strict descendant directory of `ancestor`.
///
/// `""` is treated as the root of the relative tree, so anything non-empty
/// is a strict descendant of `""`. Equality returns `false` (strict).
fn is_strict_descendant(desc: &Path, ancestor: &Path) -> bool {
    if desc == ancestor {
        return false;
    }
    if ancestor.as_os_str().is_empty() {
        return !desc.as_os_str().is_empty();
    }
    desc.starts_with(ancestor)
}

/// Build a `CrateEntry` from a `ModuleNode` — the per-node projection step.
fn node_to_crate_entry(node: &ModuleNode, rnme_lib_path: &Path) -> Result<CrateEntry, CompileError> {
    let crate_name = node.module_name.clone();
    let group_key = group_key_from_dir(&node.effective_dir);
    let original_dir = node.path.parent().unwrap_or(Path::new("."));
    let dir_str = original_dir.to_string_lossy();
    let mut lib_source = transform_source(&node.source, &group_key, &dir_str);

    // Subtasks block: only emitted when this node has descendants.
    let subtasks_block = emit_subtasks_block(node)?;
    if !subtasks_block.is_empty() {
        lib_source.push_str(&subtasks_block);
    }

    let rewritten_deps = rewrite_path_deps(&node.frontmatter.dependencies, original_dir);

    let descendant_crate_names = collect_descendant_crate_names(node);

    let mut cargo_toml = format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
name = "{crate_name}"
path = "src/lib.rs"

[dependencies]
rnme = {{ path = "{rnme_lib}" }}
"#,
        crate_name = crate_name,
        rnme_lib = rnme_lib_path.display(),
    );

    for (name, version_spec) in &rewritten_deps {
        cargo_toml.push_str(&format!("{} = {}\n", name, version_spec));
    }

    if !descendant_crate_names.is_empty() {
        cargo_toml.push_str("\n# --- subtasks (auto-generated) ---\n");
        for dep_name in &descendant_crate_names {
            cargo_toml.push_str(&format!(
                "{} = {{ path = \"../{}\" }}\n",
                dep_name, dep_name,
            ));
        }
    }

    Ok(CrateEntry {
        crate_name,
        group_key,
        lib_source,
        cargo_toml,
        descendant_crate_names,
    })
}

/// Flatten the module tree depth-first into the `CrateEntry` Vec the
/// existing generators consume. Root is emitted first, then each subtree.
fn flatten_tree_into_entries(
    node: &ModuleNode,
    rnme_lib_path: &Path,
    out: &mut Vec<CrateEntry>,
) -> Result<(), CompileError> {
    out.push(node_to_crate_entry(node, rnme_lib_path)?);
    for child in &node.children {
        flatten_tree_into_entries(child, rnme_lib_path, out)?;
    }
    debug_assert!(
        subtasks_dep_graph_is_acyclic(out),
        "subtasks injection produced a cyclic dep graph",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Subtasks module-tree emission (task #14)
// ---------------------------------------------------------------------------

/// Intermediate accumulator that merges multiple descendant paths into a
/// single rendering tree. Built per parent node.
///
/// `Intermediate` represents a directory on the path to one or more
/// descendants that has no RUNME.rs of its own — emitted as a structural
/// `pub mod` with no `pub use`. `Terminal` represents the directory that
/// contains a descendant's RUNME.rs — emitted as `pub mod <module_name> {
/// pub use ::<crate_name>::*; <recurse into its own children> }`.
///
/// Children keyed by `BTreeMap` for deterministic alphabetical output.
enum MergeNode<'a> {
    Intermediate {
        children: BTreeMap<String, MergeNode<'a>>,
    },
    Terminal {
        node: &'a ModuleNode,
        // Children of the terminal node are NOT stored here — they are
        // emitted by recursing into `emit_subtasks_body(node)` so the
        // subtree's own ordering rules apply.
    },
}

impl<'a> MergeNode<'a> {
    fn new_intermediate() -> Self {
        MergeNode::Intermediate {
            children: BTreeMap::new(),
        }
    }
}

/// Insert a single descendant into the merge tree.
///
/// `segment` is the path from the parent's `effective_dir` to the
/// descendant's `effective_dir`. Each component except the last creates
/// (or descends into) an `Intermediate` node; the last component installs
/// a `Terminal` carrying the descendant `ModuleNode`.
///
/// The intermediate component strings are run through
/// `crate_name_from_path` segment-by-segment so structural-only dir names
/// (which never see `[rnme.rename]`) still produce valid Rust identifiers.
/// The terminal uses the descendant's `module_name` directly because that
/// already incorporates any rename and the same path normalizer.
///
/// Returns `Err(CompileError::SiblingNameCollision)` if a terminal already
/// occupies the key being inserted — two sibling dirs normalized to the
/// same Rust identifier.
fn insert_descendant<'a>(
    tree: &mut MergeNode<'a>,
    segment: &Path,
    descendant: &'a ModuleNode,
) -> Result<(), CompileError> {
    let components: Vec<String> = segment
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    debug_assert!(
        !components.is_empty(),
        "descendant must have at least one path component beyond its parent",
    );

    let mut cursor = tree;
    let last_idx = components.len() - 1;
    for (i, comp) in components.iter().enumerate() {
        if i == last_idx {
            // Terminal — use the FINAL segment of the descendant's
            // effective_dir as the map key. This is just the directory
            // basename, with any rename already applied (because rename
            // swaps the basename of the final segment). The descendant's
            // `module_name` is the FLATTENED crate name (e.g.
            // `services_api_v2`) which is wrong for a nested module path.
            let key = comp.clone();
            match cursor {
                MergeNode::Intermediate { children } => {
                    // Normalize the key (same transform applied to bare dir names):
                    // dash and dot → underscore, keyword/digit guard.
                    let normalized_key = crate_name_from_path(&Path::new(&key).join("RUNME.rs"));
                    if let Some(MergeNode::Terminal { node: existing }) =
                        children.get(normalized_key.as_str())
                    {
                        // Two siblings map to the same normalized identifier.
                        // Suggest a rename for the incoming (b) path.
                        // Heuristic: if one dir uses dashes and the other uses
                        // underscores for the same base (e.g. "foo-bar" vs
                        // "foo_bar"), suggest "<basename>_dashed" for the
                        // dashed variant. Otherwise default to "<basename>_2".
                        let existing_basename = existing
                            .path
                            .parent()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let incoming_basename = descendant
                            .path
                            .parent()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let suggestion = collision_suggestion(
                            &normalized_key,
                            &existing_basename,
                            &incoming_basename,
                        );
                        return Err(CompileError::SiblingNameCollision {
                            path_a: existing.path.clone(),
                            path_b: descendant.path.clone(),
                            resolved_name: normalized_key,
                            suggestion,
                        });
                    }
                    children.insert(
                        normalized_key,
                        MergeNode::Terminal { node: descendant },
                    );
                }
                MergeNode::Terminal { .. } => {
                    // Would only happen if a previous descendant's terminal
                    // sits on the path of a deeper one — i.e. an intermediate
                    // dir that *does* have a RUNME.rs. In our `ModuleNode`
                    // tree these are emitted as separate children of the
                    // closest ancestor, so this branch is unreachable.
                }
            }
            return Ok(());
        }

        // Intermediate component: normalize the raw on-disk path component
        // through the same path-to-ident normalizer used for real nodes.
        let normalized = crate_name_from_path(&Path::new(comp).join("RUNME.rs"));
        match cursor {
            MergeNode::Intermediate { children } => {
                let next = children
                    .entry(normalized)
                    .or_insert_with(MergeNode::new_intermediate);
                cursor = next;
            }
            MergeNode::Terminal { .. } => {
                // Would only happen if a previous descendant's terminal
                // sits on the path of a deeper one — i.e. an intermediate
                // dir that *does* have a RUNME.rs. In our `ModuleNode`
                // tree these are emitted as separate children of the
                // closest ancestor, so this branch is unreachable.
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Suggest a rename for the incoming sibling that collides with an existing one.
///
/// Heuristic: if one sibling's raw on-disk basename contains dashes and the
/// other uses underscores for the same normalized base (dash/underscore are
/// the only source of Class-2 normalization collisions for ASCII names), we
/// identify the dashed one and suggest `<base>_dashed`. This gives a
/// meaningful name like `foo_bar_dashed` for `foo-bar/` colliding with
/// `foo_bar/`. In all other cases (case folding, unicode, arbitrary layout)
/// we fall back to `<base>_2` which is always unambiguous.
fn collision_suggestion(normalized: &str, existing_basename: &str, incoming_basename: &str) -> String {
    let existing_has_dashes = existing_basename.contains('-');
    let incoming_has_dashes = incoming_basename.contains('-');
    // Exactly one has dashes and the other does not → typical dash/underscore collision.
    if existing_has_dashes != incoming_has_dashes {
        format!("{}_dashed", normalized)
    } else {
        format!("{}_2", normalized)
    }
}

/// Render the merge tree into source text.
fn render_merge_node(node: &MergeNode<'_>, indent: usize) -> Result<String, CompileError> {
    let pad = "    ".repeat(indent);
    match node {
        MergeNode::Intermediate { children } => {
            let mut out = String::new();
            for (key, child) in children {
                match child {
                    MergeNode::Intermediate { .. } => {
                        out.push_str(&format!("{pad}pub mod {key} {{\n"));
                        out.push_str(&render_merge_node(child, indent + 1)?);
                        out.push_str(&format!("{pad}}}\n"));
                    }
                    MergeNode::Terminal { node } => {
                        out.push_str(&format!("{pad}pub mod {key} {{\n"));
                        out.push_str(&format!(
                            "{pad}    pub use ::{crate_name}::*;\n",
                            crate_name = node.module_name,
                        ));
                        let body = emit_subtasks_body(node, indent + 1)?;
                        if !body.is_empty() {
                            out.push_str(&body);
                        }
                        out.push_str(&format!("{pad}}}\n"));
                    }
                }
            }
            Ok(out)
        }
        // A bare top-level Terminal is never produced by build/render —
        // the top of the tree handed to render_merge_node is always an
        // Intermediate. Return empty for safety.
        MergeNode::Terminal { .. } => Ok(String::new()),
    }
}

/// Emit the body of a `mod subtasks` block for the children of `parent`.
/// Returns empty string when there are no children.
fn emit_subtasks_body(parent: &ModuleNode, indent: usize) -> Result<String, CompileError> {
    if parent.children.is_empty() {
        return Ok(String::new());
    }
    let mut tree = MergeNode::new_intermediate();
    for child in &parent.children {
        let segment = child
            .effective_dir
            .strip_prefix(&parent.effective_dir)
            .unwrap_or(child.effective_dir.as_path());
        insert_descendant(&mut tree, segment, child)?;
    }
    render_merge_node(&tree, indent)
}

/// Emit a full `mod subtasks { ... }` block for `node`, including the
/// surrounding wrapper. Returns empty string if `node` has no children.
fn emit_subtasks_block(node: &ModuleNode) -> Result<String, CompileError> {
    if node.children.is_empty() {
        return Ok(String::new());
    }
    let body = emit_subtasks_body(node, 1)?;
    let mut out = String::new();
    out.push_str("\n// === subtasks (auto-generated) ===\n");
    out.push_str("#[allow(unused_imports, dead_code)]\n");
    out.push_str("mod subtasks {\n");
    out.push_str(&body);
    out.push_str("}\n");
    Ok(out)
}

/// Collect transitive descendant crate names (depth-first).
fn collect_descendant_crate_names(node: &ModuleNode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    fn walk(n: &ModuleNode, out: &mut Vec<String>) {
        for child in &n.children {
            out.push(child.module_name.clone());
            walk(child, out);
        }
    }
    walk(node, &mut out);
    out
}

/// Verify the subtasks dep graph (parent crate → its descendant crates)
/// is acyclic. Walks the `descendant_crate_names` Vec on each entry; a
/// cycle means some parent's descendant list (transitively) reaches back
/// to that parent.
///
/// Structurally guaranteed by construction (descendants are a partial
/// order), so this only ever fires if an earlier invariant broke. Used in
/// `debug_assert!` so release builds skip it.
fn subtasks_dep_graph_is_acyclic(entries: &[CrateEntry]) -> bool {
    #[derive(Clone, Copy)]
    enum Color {
        Visiting,
        Done,
    }

    let by_name: HashMap<&str, &CrateEntry> = entries
        .iter()
        .map(|e| (e.crate_name.as_str(), e))
        .collect();

    fn dfs<'a>(
        name: &'a str,
        by_name: &HashMap<&'a str, &'a CrateEntry>,
        color: &mut HashMap<&'a str, Color>,
    ) -> bool {
        match color.get(name) {
            Some(Color::Visiting) => return false,
            Some(Color::Done) => return true,
            None => {}
        }
        color.insert(name, Color::Visiting);
        if let Some(entry) = by_name.get(name) {
            for dep in &entry.descendant_crate_names {
                if let Some((dep_key, _)) = by_name.get_key_value(dep.as_str()) {
                    if !dfs(dep_key, by_name, color) {
                        return false;
                    }
                }
            }
        }
        color.insert(name, Color::Done);
        true
    }

    let mut color: HashMap<&str, Color> = HashMap::new();
    for entry in entries {
        if !dfs(entry.crate_name.as_str(), &by_name, &mut color) {
            return false;
        }
    }
    true
}

/// Compute the group key string from a directory path.
///
/// Normalizes:
/// - Empty / `"."` → `""` (root group)
/// - Strips a leading `"./"`
/// - Trims a trailing `"/"`
fn group_key_from_dir(dir: &Path) -> String {
    let raw = dir.to_string_lossy().to_string();
    let stripped = raw.strip_prefix("./").unwrap_or(&raw);
    let trimmed = stripped.trim_end_matches('/');
    if trimmed == "." { String::new() } else { trimmed.to_string() }
}

/// Generate the full workspace structure on disk.
fn generate_workspace(
    cache_dir: &Path,
    entries: &[CrateEntry],
    rnme_lib_path: &Path,
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
rnme = {{ path = "{}" }}
"#,
        rnme_lib_path.display(),
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
    let mut members: Vec<String> = entries
        .iter()
        .map(|e| format!("\"{}\"", e.crate_name))
        .collect();
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

/// Compute the cache directory for a root RUNME.rs file.
///
/// Hashes the absolute path to produce a stable, filesystem-safe directory name.
fn cache_dir_for_root(root_abs: &Path) -> Result<PathBuf, CompileError> {
    let home = home_dir().ok_or(CompileError::NoHomeDir)?;
    let mut hasher = Sha256::new();
    hasher.update(root_abs.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let hash_prefix = &hash[..HASH_PREFIX_LEN];
    Ok(home.join(".cache").join("rnme").join(hash_prefix))
}

/// Find the absolute path to the `rnme` library crate.
///
/// Strategy: walk up from the binary's location or the RUNME.rs file's directory
/// looking for a Cargo.toml with `name = "rnme"`. With the merged crate layout,
/// the library is at the workspace root.
fn find_rnme_lib_path(rnme_file: &Path) -> Result<PathBuf, CompileError> {
    // First, try to find it relative to the rnme binary's location.
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let mut search = exe_dir.to_path_buf();
        loop {
            if looks_like_rnme_root(&search) {
                return Ok(search);
            }
            if !search.pop() {
                break;
            }
        }
    }

    // Fallback: walk up from the RUNME.rs file location
    let start_dir = rnme_file.parent().ok_or(CompileError::NoLibPath)?;
    let mut search = start_dir.to_path_buf();
    loop {
        if looks_like_rnme_root(&search) {
            return Ok(search);
        }
        if !search.pop() {
            break;
        }
    }

    // Last resort: check if RNME_LIB_PATH env var is set
    if let Ok(path) = std::env::var("RNME_LIB_PATH") {
        let p = PathBuf::from(path);
        if p.join("Cargo.toml").is_file() {
            return Ok(p);
        }
    }

    Err(CompileError::NoLibPath)
}

/// Check if a directory looks like the rnme workspace root.
fn looks_like_rnme_root(dir: &Path) -> bool {
    let cargo = dir.join("Cargo.toml");
    if let Ok(content) = fs::read_to_string(&cargo) {
        content.contains("name = \"rnme\"") && dir.join("src").join("lib.rs").is_file()
    } else {
        false
    }
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
        assert!(dir.to_string_lossy().contains(".cache/rnme/"));
    }

    #[test]
    fn test_generate_runner_main_contains_link_calls() {
        let entries = vec![
            CrateEntry {
                crate_name: "root".to_string(),
                group_key: "".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
                descendant_crate_names: Vec::new(),
            },
            CrateEntry {
                crate_name: "services_auth".to_string(),
                group_key: "services/auth".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
                descendant_crate_names: Vec::new(),
            },
        ];
        let main_rs = generate_runner_main(&entries);
        assert!(main_rs.contains("root::__rnme_link();"));
        assert!(main_rs.contains("services_auth::__rnme_link();"));
        assert!(main_rs.contains("Registry::from_inventory()"));
        assert!(main_rs.contains("fn main()"));
    }

    #[test]
    fn test_generate_runner_main_init_ordering() {
        let entries = vec![CrateEntry {
            crate_name: "root".to_string(),
            group_key: "".to_string(),
            lib_source: String::new(),
            cargo_toml: String::new(),
            descendant_crate_names: Vec::new(),
        }];
        let main_rs = generate_runner_main(&entries);
        // Verify that init sorting logic is present (leaf-to-root)
        assert!(main_rs.contains("depth_b.cmp(&depth_a)"));
    }

    /// Run the full root → CrateEntry pipeline used by the production code:
    /// build the module tree, then flatten depth-first into entries. Root
    /// first, then children depth-first.
    fn process_files_via_tree(
        root_rnme: &Path,
        children: &[PathBuf],
        rnme_lib: &Path,
    ) -> Vec<CrateEntry> {
        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.to_path_buf()),
            children: children.to_vec(),
        };
        let root_dir = root_rnme.parent().unwrap();
        let tree = build_module_tree(&discovery, root_dir).unwrap();
        let mut entries: Vec<CrateEntry> = Vec::new();
        flatten_tree_into_entries(&tree, rnme_lib, &mut entries).unwrap();
        entries
    }

    #[test]
    fn test_root_only_basic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rnme_path = tmp.path().join("RUNME.rs");
        fs::write(&rnme_path, "fn hello() {}\n").unwrap();

        let rnme_lib = rnme_lib_path();
        let entries = process_files_via_tree(&rnme_path, &[], &rnme_lib);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.crate_name, "root");
        assert_eq!(entry.group_key, "");
        assert!(entry.lib_source.contains("const __RNME_GROUP: &str = \"\";"));
        assert!(entry.lib_source.contains("pub fn __rnme_link() {}"));
        assert!(entry.cargo_toml.contains("name = \"root\""));
    }

    #[test]
    fn test_nested_child_basic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn deploy() {}\n").unwrap();

        let child_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(&child_rnme, "fn migrate() {}\n").unwrap();

        let rnme_lib = rnme_lib_path();
        let entries = process_files_via_tree(&root_rnme, &[child_rnme.clone()], &rnme_lib);
        let auth = entries
            .iter()
            .find(|e| e.crate_name == "services_auth")
            .expect("services_auth entry missing");
        assert_eq!(auth.group_key, "services/auth");
        assert!(
            auth.lib_source
                .contains("const __RNME_GROUP: &str = \"services/auth\";")
        );
    }

    // -----------------------------------------------------------------------
    // [rnme.rename] propagation tests — tree traversal (apply-rename, task #9)
    // -----------------------------------------------------------------------

    #[test]
    fn test_group_key_from_dir_root() {
        assert_eq!(group_key_from_dir(Path::new("")), "");
        assert_eq!(group_key_from_dir(Path::new(".")), "");
        assert_eq!(group_key_from_dir(Path::new("./")), "");
    }

    #[test]
    fn test_group_key_from_dir_nested() {
        assert_eq!(group_key_from_dir(Path::new("services/auth")), "services/auth");
        assert_eq!(group_key_from_dir(Path::new("./services/auth")), "services/auth");
        assert_eq!(group_key_from_dir(Path::new("services/auth/")), "services/auth");
    }

    #[test]
    fn test_build_module_tree_root_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        assert_eq!(tree.module_name, "root");
        assert_eq!(tree.effective_dir, PathBuf::new());
        assert!(tree.children.is_empty());
    }

    #[test]
    fn test_build_module_tree_root_with_rename_is_ignored() {
        // Root carries `[rnme.rename]`; the rename is never consulted
        // because root is never visited as a child. Result: root keeps
        // its assigned name "root" and no error is produced.
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(
            &root_rnme,
            "//! [rnme.rename]\n//! name = \"monorepo\"\n\nfn task() {}\n",
        )
        .unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        assert_eq!(tree.module_name, "root");
        assert_eq!(tree.effective_dir, PathBuf::new());
    }

    #[test]
    fn test_build_module_tree_child_with_rename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("foo");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(
            &child_rnme,
            "//! [rnme.rename]\n//! name = \"foo_bar_v2\"\n",
        )
        .unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![child_rnme.clone()],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        assert_eq!(tree.children.len(), 1);
        let child = &tree.children[0];
        assert_eq!(child.module_name, "foo_bar_v2");
        assert_eq!(child.effective_dir, PathBuf::from("foo_bar_v2"));
    }

    #[test]
    fn test_build_module_tree_nested_child_with_rename() {
        // Layout: root + services/auth/RUNME.rs with rename "auth_v2".
        // No services/RUNME.rs, so services/auth attaches to root.
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(&child_rnme, "//! [rnme.rename]\n//! name = \"auth_v2\"\n").unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![child_rnme.clone()],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        assert_eq!(tree.children.len(), 1);
        let child = &tree.children[0];
        // module_name reflects the rename + the full path normalization
        assert_eq!(child.module_name, "services_auth_v2");
        // effective_dir is the substituted-path equivalent (slashes preserved)
        assert_eq!(child.effective_dir, PathBuf::from("services/auth_v2"));
    }

    #[test]
    fn test_build_module_tree_child_rename_heck_normalizes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("foo");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(
            &child_rnme,
            "//! [rnme.rename]\n//! name = \"Hello World\"\n",
        )
        .unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![child_rnme.clone()],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        let child = &tree.children[0];
        assert_eq!(child.module_name, "hello_world");
        assert_eq!(child.effective_dir, PathBuf::from("hello_world"));
    }

    #[test]
    fn test_build_module_tree_child_rename_camel_case() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("foo");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(&child_rnme, "//! [rnme.rename]\n//! name = \"FooBar\"\n").unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![child_rnme.clone()],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        assert_eq!(tree.children[0].module_name, "foo_bar");
    }

    #[test]
    fn test_build_module_tree_unicode_path_unchanged() {
        // No rename on the child; heck is NOT applied to path-derived inputs.
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("café");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(&child_rnme, "fn task() {}\n").unwrap();

        let discovery = DiscoveryResult {
            nearest: Some(root_rnme.clone()),
            children: vec![child_rnme.clone()],
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        assert_eq!(tree.children[0].module_name, "café");
    }

    #[test]
    fn test_workspace_emits_renamed_child() {
        // Three-way agreement: cargo crate name, __RNME_GROUP, and the
        // workspace members entry all reflect the rename.
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("foo");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(
            &child_rnme,
            "//! [rnme.rename]\n//! name = \"foo_bar_v2\"\n",
        )
        .unwrap();

        let rnme_lib = rnme_lib_path();
        let entries = process_files_via_tree(&root_rnme, &[child_rnme.clone()], &rnme_lib);
        let renamed = entries
            .iter()
            .find(|e| e.crate_name == "foo_bar_v2")
            .expect("renamed entry missing");
        assert_eq!(renamed.group_key, "foo_bar_v2");
        assert!(
            renamed
                .lib_source
                .contains("const __RNME_GROUP: &str = \"foo_bar_v2\";")
        );
        assert!(renamed.cargo_toml.contains("name = \"foo_bar_v2\""));
    }

    #[test]
    fn test_workspace_renamed_nested() {
        // Same three-way agreement, but with a nested child + path prefix.
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let child_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&child_dir).unwrap();
        let child_rnme = child_dir.join("RUNME.rs");
        fs::write(&child_rnme, "//! [rnme.rename]\n//! name = \"auth_v2\"\n").unwrap();

        let rnme_lib = rnme_lib_path();
        let entries = process_files_via_tree(&root_rnme, &[child_rnme.clone()], &rnme_lib);
        let renamed = entries
            .iter()
            .find(|e| e.crate_name == "services_auth_v2")
            .expect("renamed nested entry missing");
        assert_eq!(renamed.group_key, "services/auth_v2");
        assert!(
            renamed
                .lib_source
                .contains("const __RNME_GROUP: &str = \"services/auth_v2\";")
        );
    }

    #[test]
    fn test_workspace_root_rename_observably_noop() {
        // Root with `[rnme.rename]` produces a root entry whose crate name
        // is still "root" — no error, no propagation of the rename.
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(
            &root_rnme,
            "//! [rnme.rename]\n//! name = \"ignored\"\n\nfn task() {}\n",
        )
        .unwrap();

        let rnme_lib = rnme_lib_path();
        let entries = process_files_via_tree(&root_rnme, &[], &rnme_lib);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].crate_name, "root");
        assert_eq!(entries[0].group_key, "");
        assert!(
            entries[0]
                .lib_source
                .contains("const __RNME_GROUP: &str = \"\";")
        );
    }

    // -----------------------------------------------------------------------
    // subtasks-injection (task #14) tests
    // -----------------------------------------------------------------------

    /// Build a `ModuleNode` tree from a layout described by RUNME paths
    /// relative to the temp root. Each `(path, source)` pair becomes a
    /// RUNME.rs on disk; `build_module_tree` then traverses it.
    fn make_tree(layout: &[(&str, &str)]) -> (tempfile::TempDir, ModuleNode) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut children: Vec<PathBuf> = Vec::new();
        let mut root_path: Option<PathBuf> = None;
        for (rel, source) in layout {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, source).unwrap();
            if rel == &"RUNME.rs" {
                root_path = Some(p);
            } else {
                children.push(p);
            }
        }
        let root = root_path.expect("layout must contain a root RUNME.rs");
        let discovery = DiscoveryResult {
            nearest: Some(root.clone()),
            children,
        };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        (tmp, tree)
    }

    #[test]
    fn test_subtasks_empty_for_leaf() {
        let (_tmp, tree) = make_tree(&[("RUNME.rs", "fn task() {}\n")]);
        assert_eq!(emit_subtasks_block(&tree).unwrap(), "");
    }

    #[test]
    fn test_subtasks_single_child() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo/RUNME.rs", "fn task() {}\n"),
        ]);
        let block = emit_subtasks_block(&tree).unwrap();
        assert!(block.contains("mod subtasks {"), "got: {block}");
        assert!(block.contains("pub mod foo {"), "got: {block}");
        assert!(block.contains("pub use ::foo::*;"), "got: {block}");
    }

    #[test]
    fn test_subtasks_block_is_not_pub() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo/RUNME.rs", "fn task() {}\n"),
        ]);
        let block = emit_subtasks_block(&tree).unwrap();
        assert!(block.contains("mod subtasks {"), "got: {block}");
        assert!(!block.contains("pub mod subtasks"), "got: {block}");
    }

    #[test]
    fn test_subtasks_structural_only_intermediate() {
        // structural_only/ has no RUNME.rs; only structural_only/leaf/RUNME.rs.
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("structural_only/leaf/RUNME.rs", "fn task() {}\n"),
        ]);
        let block = emit_subtasks_block(&tree).unwrap();
        // Outer structural module has no pub use.
        assert!(block.contains("pub mod structural_only {"), "got: {block}");
        // Inner terminal carries the pub use of the leaf crate.
        assert!(block.contains("pub mod leaf {"), "got: {block}");
        assert!(
            block.contains("pub use ::structural_only_leaf::*;"),
            "got: {block}"
        );
        // Critically: the structural_only line is NOT followed by a pub use.
        // We check the structural intermediate has no `pub use ::structural_only::`.
        assert!(
            !block.contains("pub use ::structural_only::"),
            "structural-only intermediate must not have a `pub use`; got: {block}"
        );
    }

    #[test]
    fn test_subtasks_nested_intermediate_with_rnme() {
        // services/RUNME.rs is a real intermediate; services/api/RUNME.rs is
        // its child.
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("services/RUNME.rs", "fn task() {}\n"),
            ("services/api/RUNME.rs", "fn task() {}\n"),
        ]);
        // Root's emission: one `pub mod services` carrying both its own
        // pub use and a nested `pub mod api { pub use ::services_api::*; }`.
        let root_block = emit_subtasks_block(&tree).unwrap();
        assert!(root_block.contains("pub mod services {"), "got: {root_block}");
        assert!(root_block.contains("pub use ::services::*;"), "got: {root_block}");
        assert!(root_block.contains("pub mod api {"), "got: {root_block}");
        assert!(
            root_block.contains("pub use ::services_api::*;"),
            "got: {root_block}"
        );

        // services' own subtasks block has ONLY api, no `services` outer
        // wrapper, no `pub use ::services::*` (that's the parent's import).
        let services_node = tree
            .children
            .iter()
            .find(|c| c.module_name == "services")
            .expect("services child");
        let services_block = emit_subtasks_block(services_node).unwrap();
        assert!(services_block.contains("pub mod api {"), "got: {services_block}");
        assert!(
            services_block.contains("pub use ::services_api::*;"),
            "got: {services_block}"
        );
        assert!(
            !services_block.contains("pub use ::services::*;"),
            "services' own subtasks must not re-export itself; got: {services_block}"
        );
    }

    #[test]
    fn test_subtasks_renamed_basename_emits_renamed_ident() {
        // foo/RUNME.rs renames itself to "bar".
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            (
                "foo/RUNME.rs",
                "//! [rnme.rename]\n//! name = \"bar\"\n",
            ),
        ]);
        let block = emit_subtasks_block(&tree).unwrap();
        assert!(block.contains("pub mod bar {"), "got: {block}");
        assert!(block.contains("pub use ::bar::*;"), "got: {block}");
        assert!(!block.contains("pub mod foo"), "old name must be gone; got: {block}");
    }

    #[test]
    fn test_subtasks_sibling_structural_paths_merge() {
        // Two leaves under a shared structural-only `a/b/`. Expected: one
        // `pub mod a { pub mod b { pub mod c {...} pub mod d {...} } }`,
        // NOT two duplicate `pub mod a` siblings.
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("a/b/c/RUNME.rs", "fn task() {}\n"),
            ("a/b/d/RUNME.rs", "fn task() {}\n"),
        ]);
        let block = emit_subtasks_block(&tree).unwrap();
        // `pub mod a {` should appear exactly once.
        assert_eq!(
            block.matches("pub mod a {").count(),
            1,
            "structural `a` must be merged into a single block; got: {block}"
        );
        // `pub mod b {` should also appear exactly once.
        assert_eq!(
            block.matches("pub mod b {").count(),
            1,
            "structural `b` must be merged into a single block; got: {block}"
        );
        assert!(block.contains("pub mod c {"), "got: {block}");
        assert!(block.contains("pub mod d {"), "got: {block}");
    }

    #[test]
    fn test_subtasks_not_emitted_for_no_children() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo/RUNME.rs", "fn task() {}\n"),
        ]);
        // foo has no children — its subtasks block must be empty.
        let foo = &tree.children[0];
        assert_eq!(emit_subtasks_block(foo).unwrap(), "");
    }

    #[test]
    fn test_subtasks_deterministic_ordering() {
        // BTreeMap pins alphabetical ordering. Verify two siblings come out
        // in alphabetical order regardless of discovery sequence.
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("zeta/RUNME.rs", "fn task() {}\n"),
            ("alpha/RUNME.rs", "fn task() {}\n"),
        ]);
        let block = emit_subtasks_block(&tree).unwrap();
        let alpha_idx = block.find("pub mod alpha").expect("alpha present");
        let zeta_idx = block.find("pub mod zeta").expect("zeta present");
        assert!(alpha_idx < zeta_idx, "alphabetical order; got: {block}");
    }

    // ---- Collision detection ----

    /// Helper: build the module tree and attempt to emit the subtasks block,
    /// returning the error if one fires.
    fn try_emit_subtasks(layout: &[(&str, &str)]) -> Result<String, CompileError> {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut children: Vec<PathBuf> = Vec::new();
        let mut root_path: Option<PathBuf> = None;
        for (rel, source) in layout {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, source).unwrap();
            if rel == &"RUNME.rs" {
                root_path = Some(p);
            } else {
                children.push(p);
            }
        }
        let root = root_path.expect("layout must contain a root RUNME.rs");
        let discovery = DiscoveryResult { nearest: Some(root.clone()), children };
        let tree = build_module_tree(&discovery, tmp.path()).unwrap();
        emit_subtasks_block(&tree)
    }

    #[test]
    fn test_sibling_collision_dash_vs_underscore_errors() {
        // foo-bar/ and foo_bar/ both normalize to `foo_bar` — collision.
        let result = try_emit_subtasks(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo-bar/RUNME.rs", "fn task() {}\n"),
            ("foo_bar/RUNME.rs", "fn task() {}\n"),
        ]);
        let err = result.expect_err("expected SiblingNameCollision");
        match &err {
            CompileError::SiblingNameCollision { resolved_name, suggestion, .. } => {
                assert_eq!(resolved_name, "foo_bar");
                // One has dashes, other has underscores → suggestion is foo_bar_dashed.
                assert_eq!(suggestion, "foo_bar_dashed");
            }
            other => panic!("expected SiblingNameCollision, got: {other}"),
        }
        // Error message includes the resolved name and the frontmatter snippet.
        let msg = err.to_string();
        assert!(msg.contains("foo_bar"), "msg: {msg}");
        assert!(msg.contains("[rnme.rename]"), "msg: {msg}");
        assert!(msg.contains("foo_bar_dashed"), "msg: {msg}");
    }

    #[test]
    fn test_sibling_collision_resolved_by_rename() {
        // Same layout, but foo-bar/ adds [rnme.rename] — no collision.
        let result = try_emit_subtasks(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo-bar/RUNME.rs", "//! [rnme.rename]\n//! name = \"foo_bar_dashed\"\n"),
            ("foo_bar/RUNME.rs", "fn task() {}\n"),
        ]);
        let block = result.expect("rename resolves the collision");
        assert!(block.contains("pub mod foo_bar {"), "got: {block}");
        assert!(block.contains("pub mod foo_bar_dashed {"), "got: {block}");
    }

    // ---- Cargo.toml extension ----

    #[test]
    fn test_cargo_toml_descendant_path_deps_direct() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo/RUNME.rs", "fn task() {}\n"),
        ]);
        let rnme_lib = rnme_lib_path();
        let root_entry = node_to_crate_entry(&tree, &rnme_lib).unwrap();
        assert!(
            root_entry.cargo_toml.contains("foo = { path = \"../foo\" }"),
            "got: {}",
            root_entry.cargo_toml
        );
        assert_eq!(root_entry.descendant_crate_names, vec!["foo".to_string()]);
    }

    #[test]
    fn test_cargo_toml_descendant_path_deps_transitive() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("services/RUNME.rs", "fn task() {}\n"),
            ("services/api/RUNME.rs", "fn task() {}\n"),
        ]);
        let rnme_lib = rnme_lib_path();
        let root_entry = node_to_crate_entry(&tree, &rnme_lib).unwrap();
        // Root depends on both services and services_api (transitive).
        assert!(
            root_entry
                .cargo_toml
                .contains("services = { path = \"../services\" }"),
            "got: {}",
            root_entry.cargo_toml
        );
        assert!(
            root_entry
                .cargo_toml
                .contains("services_api = { path = \"../services_api\" }"),
            "got: {}",
            root_entry.cargo_toml
        );
        // services itself depends ONLY on services_api, not root.
        let services_node = tree
            .children
            .iter()
            .find(|c| c.module_name == "services")
            .unwrap();
        let services_entry = node_to_crate_entry(services_node, &rnme_lib).unwrap();
        assert_eq!(
            services_entry.descendant_crate_names,
            vec!["services_api".to_string()]
        );
        assert!(
            !services_entry.cargo_toml.contains("\"../root\""),
            "services must not depend on root; got: {}",
            services_entry.cargo_toml
        );
    }

    #[test]
    fn test_cargo_toml_no_deps_for_leaf() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("foo/RUNME.rs", "fn task() {}\n"),
        ]);
        let rnme_lib = rnme_lib_path();
        let foo = &tree.children[0];
        let foo_entry = node_to_crate_entry(foo, &rnme_lib).unwrap();
        assert!(foo_entry.descendant_crate_names.is_empty());
        // Auto-generated marker should be absent.
        assert!(
            !foo_entry
                .cargo_toml
                .contains("subtasks (auto-generated)"),
            "leaf must not have the marker block; got: {}",
            foo_entry.cargo_toml
        );
    }

    #[test]
    fn test_cargo_toml_renamed_descendant_uses_renamed_crate_name() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            (
                "foo/RUNME.rs",
                "//! [rnme.rename]\n//! name = \"bar\"\n",
            ),
        ]);
        let rnme_lib = rnme_lib_path();
        let root_entry = node_to_crate_entry(&tree, &rnme_lib).unwrap();
        assert!(
            root_entry.cargo_toml.contains("bar = { path = \"../bar\" }"),
            "got: {}",
            root_entry.cargo_toml
        );
        assert!(
            !root_entry.cargo_toml.contains("foo = { path =") ,
            "old crate name must be gone; got: {}",
            root_entry.cargo_toml
        );
    }

    // ---- Cycle guard ----

    #[test]
    fn test_subtasks_dep_graph_acyclic_on_valid_tree() {
        let (_tmp, tree) = make_tree(&[
            ("RUNME.rs", "fn task() {}\n"),
            ("services/RUNME.rs", "fn task() {}\n"),
            ("services/api/RUNME.rs", "fn task() {}\n"),
            ("foo/RUNME.rs", "fn task() {}\n"),
        ]);
        let rnme_lib = rnme_lib_path();
        let mut entries: Vec<CrateEntry> = Vec::new();
        flatten_tree_into_entries(&tree, &rnme_lib, &mut entries).unwrap();
        assert!(subtasks_dep_graph_is_acyclic(&entries));
    }

    #[test]
    fn test_subtasks_dep_graph_detects_cycle() {
        // Hand-construct a clearly cyclic graph.
        let entries = vec![
            CrateEntry {
                crate_name: "a".to_string(),
                group_key: "a".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
                descendant_crate_names: vec!["b".to_string()],
            },
            CrateEntry {
                crate_name: "b".to_string(),
                group_key: "b".to_string(),
                lib_source: String::new(),
                cargo_toml: String::new(),
                descendant_crate_names: vec!["a".to_string()],
            },
        ];
        assert!(!subtasks_dep_graph_is_acyclic(&entries));
    }

    // ---- End-to-end fs-driven integration ----

    #[test]
    fn test_integration_subtasks_full_pipeline() {
        // Layout: root + services/ (real) + services/api/ (real, renamed
        // to api_v2) + structural_only/leaf/ (leaf under structural).
        let tmp = tempfile::TempDir::new().unwrap();
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn task() {}\n").unwrap();

        let services_dir = tmp.path().join("services");
        fs::create_dir_all(&services_dir).unwrap();
        let services_rnme = services_dir.join("RUNME.rs");
        fs::write(&services_rnme, "fn task() {}\n").unwrap();

        let api_dir = services_dir.join("api");
        fs::create_dir_all(&api_dir).unwrap();
        let api_rnme = api_dir.join("RUNME.rs");
        fs::write(
            &api_rnme,
            "//! [rnme.rename]\n//! name = \"api_v2\"\n",
        )
        .unwrap();

        let leaf_dir = tmp.path().join("structural_only").join("leaf");
        fs::create_dir_all(&leaf_dir).unwrap();
        let leaf_rnme = leaf_dir.join("RUNME.rs");
        fs::write(&leaf_rnme, "fn task() {}\n").unwrap();

        let rnme_lib = rnme_lib_path();
        let entries = process_files_via_tree(
            &root_rnme,
            &[
                services_rnme.clone(),
                api_rnme.clone(),
                leaf_rnme.clone(),
            ],
            &rnme_lib,
        );

        // Generate the workspace on disk.
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &rnme_lib).unwrap();

        // Root lib.rs has the expected subtasks block.
        let root_lib = fs::read_to_string(cache_dir.join("root/src/lib.rs")).unwrap();
        assert!(root_lib.contains("mod subtasks {"), "got:\n{}", root_lib);
        assert!(
            root_lib.contains("pub use ::services::*;"),
            "got:\n{}",
            root_lib
        );
        assert!(
            root_lib.contains("pub mod api_v2 {"),
            "renamed module path; got:\n{}",
            root_lib
        );
        assert!(
            root_lib.contains("pub use ::services_api_v2::*;"),
            "renamed crate import; got:\n{}",
            root_lib
        );
        assert!(
            root_lib.contains("pub mod structural_only {"),
            "structural intermediate; got:\n{}",
            root_lib
        );
        assert!(
            root_lib.contains("pub mod leaf {"),
            "leaf terminal; got:\n{}",
            root_lib
        );
        assert!(
            root_lib.contains("pub use ::structural_only_leaf::*;"),
            "leaf crate import; got:\n{}",
            root_lib
        );

        // Root Cargo.toml has transitive path-deps on all three descendant
        // crates.
        let root_toml = fs::read_to_string(cache_dir.join("root/Cargo.toml")).unwrap();
        assert!(
            root_toml.contains("services = { path = \"../services\" }"),
            "got:\n{}",
            root_toml
        );
        assert!(
            root_toml.contains("services_api_v2 = { path = \"../services_api_v2\" }"),
            "got:\n{}",
            root_toml
        );
        assert!(
            root_toml.contains("structural_only_leaf = { path = \"../structural_only_leaf\" }"),
            "got:\n{}",
            root_toml
        );

        // Intermediate `services` lib.rs has a smaller subtasks block,
        // scoped to its own subtree (just api_v2, no leaf, no services).
        let services_lib =
            fs::read_to_string(cache_dir.join("services/src/lib.rs")).unwrap();
        assert!(
            services_lib.contains("mod subtasks {"),
            "got:\n{}",
            services_lib
        );
        assert!(
            services_lib.contains("pub mod api_v2 {"),
            "got:\n{}",
            services_lib
        );
        assert!(
            services_lib.contains("pub use ::services_api_v2::*;"),
            "got:\n{}",
            services_lib
        );
        assert!(
            !services_lib.contains("pub mod leaf"),
            "services must not see structural_only/leaf; got:\n{}",
            services_lib
        );

        // Leaf crates: no mod subtasks at all.
        let api_lib =
            fs::read_to_string(cache_dir.join("services_api_v2/src/lib.rs")).unwrap();
        assert!(
            !api_lib.contains("mod subtasks"),
            "leaf must not have subtasks; got:\n{}",
            api_lib
        );
    }

    #[test]
    fn test_generate_workspace_structure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache_dir = tmp.path().join("workspace");
        fs::create_dir_all(&cache_dir).unwrap();

        let rnme_lib = PathBuf::from("/fake/path/to/rnme");

        let entries = vec![
            CrateEntry {
                crate_name: "root".to_string(),
                group_key: "".to_string(),
                lib_source: "// root lib".to_string(),
                cargo_toml: "[package]\nname = \"root\"\n".to_string(),
                descendant_crate_names: Vec::new(),
            },
            CrateEntry {
                crate_name: "services_auth".to_string(),
                group_key: "services/auth".to_string(),
                lib_source: "// auth lib".to_string(),
                cargo_toml: "[package]\nname = \"services_auth\"\n".to_string(),
                descendant_crate_names: Vec::new(),
            },
        ];

        generate_workspace(&cache_dir, &entries, &rnme_lib).unwrap();

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
        assert!(runner_main.contains("root::__rnme_link();"));
        assert!(runner_main.contains("services_auth::__rnme_link();"));
    }

    // -----------------------------------------------------------------------
    // Integration tests: full pipeline from RUNME.rs files → workspace
    // -----------------------------------------------------------------------

    /// Helper: resolve the rnme library path (the workspace root).
    fn rnme_lib_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Test 1: Single-file workspace generation.
    ///
    /// Creates a temp dir with one RUNME.rs, builds a DiscoveryResult,
    /// processes files, generates workspace, and verifies structure.
    #[test]
    fn test_integration_single_file_workspace() {
        let rnme_lib = rnme_lib_path();

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a single RUNME.rs at root
        let rnme_path = tmp.path().join("RUNME.rs");
        fs::write(&rnme_path, "\nfn hello() {}\n").unwrap();

        let entries = process_files_via_tree(&rnme_path, &[], &rnme_lib);

        // Should have exactly 1 entry
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].crate_name, "root");
        assert_eq!(entries[0].group_key, "");

        // Generate workspace
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &rnme_lib).unwrap();

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

        // Verify lib.rs has __RNME_GROUP injected with empty group for root
        let lib_rs = fs::read_to_string(cache_dir.join("root/src/lib.rs")).unwrap();
        assert!(
            lib_rs.contains("const __RNME_GROUP: &str = \"\";"),
            "lib.rs should contain __RNME_GROUP injection, got: {}",
            lib_rs,
        );
        assert!(lib_rs.contains("pub fn __rnme_link() {}"));

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
        assert_eq!(
            members.len(),
            2,
            "Expected root + runner dirs, got: {:?}",
            members
        );
        assert!(members.contains(&"root".to_string()));
        assert!(members.contains(&"runner".to_string()));
    }

    /// Test 2: Multi-file workspace structure.
    ///
    /// Creates 3 RUNME.rs files (root, services/auth, web), processes them,
    /// generates workspace, and verifies 3 lib crates + runner with correct
    /// crate names and __RNME_GROUP values.
    #[test]
    fn test_integration_multi_file_workspace() {
        let rnme_lib = rnme_lib_path();

        let tmp = tempfile::TempDir::new().unwrap();

        // Create root RUNME.rs
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn deploy() {}\n").unwrap();

        // Create services/auth/RUNME.rs
        let auth_dir = tmp.path().join("services").join("auth");
        fs::create_dir_all(&auth_dir).unwrap();
        let auth_rnme = auth_dir.join("RUNME.rs");
        fs::write(&auth_rnme, "fn migrate() {}\n").unwrap();

        // Create web/RUNME.rs
        let web_dir = tmp.path().join("web");
        fs::create_dir_all(&web_dir).unwrap();
        let web_rnme = web_dir.join("RUNME.rs");
        fs::write(&web_rnme, "fn build() {}\n").unwrap();

        let entries = process_files_via_tree(
            &root_rnme,
            &[auth_rnme.clone(), web_rnme.clone()],
            &rnme_lib,
        );

        // Should have 3 entries
        assert_eq!(entries.len(), 3);

        // Verify crate names
        let crate_names: Vec<&str> = entries.iter().map(|e| e.crate_name.as_str()).collect();
        assert!(
            crate_names.contains(&"root"),
            "Expected 'root' in crate names: {:?}",
            crate_names
        );
        assert!(
            crate_names.contains(&"services_auth"),
            "Expected 'services_auth' in crate names: {:?}",
            crate_names
        );
        assert!(
            crate_names.contains(&"web"),
            "Expected 'web' in crate names: {:?}",
            crate_names
        );

        // Verify group keys
        let root_entry = entries.iter().find(|e| e.crate_name == "root").unwrap();
        assert_eq!(root_entry.group_key, "");
        let auth_entry = entries
            .iter()
            .find(|e| e.crate_name == "services_auth")
            .unwrap();
        assert_eq!(auth_entry.group_key, "services/auth");
        let web_entry = entries.iter().find(|e| e.crate_name == "web").unwrap();
        assert_eq!(web_entry.group_key, "web");

        // Generate workspace
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &rnme_lib).unwrap();

        // Verify 3 lib crates + runner generated
        assert!(cache_dir.join("root/src/lib.rs").is_file());
        assert!(cache_dir.join("services_auth/src/lib.rs").is_file());
        assert!(cache_dir.join("web/src/lib.rs").is_file());
        assert!(cache_dir.join("runner/src/main.rs").is_file());

        // Verify each lib.rs has correct __RNME_GROUP value
        let root_lib = fs::read_to_string(cache_dir.join("root/src/lib.rs")).unwrap();
        assert!(
            root_lib.contains("const __RNME_GROUP: &str = \"\";"),
            "root lib.rs should have empty group, got: {}",
            root_lib,
        );

        let auth_lib = fs::read_to_string(cache_dir.join("services_auth/src/lib.rs")).unwrap();
        assert!(
            auth_lib.contains("const __RNME_GROUP: &str = \"services/auth\";"),
            "auth lib.rs should have services/auth group, got: {}",
            auth_lib,
        );

        let web_lib = fs::read_to_string(cache_dir.join("web/src/lib.rs")).unwrap();
        assert!(
            web_lib.contains("const __RNME_GROUP: &str = \"web\";"),
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
        let rnme_lib = rnme_lib_path();

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
        let rnme_path = project_dir.join("RUNME.rs");
        fs::write(
            &rnme_path,
            r#"//! [dependencies]
//! tools = { path = "../shared/tools" }

fn build() {}
"#,
        )
        .unwrap();

        // Process the file via the new tree pipeline (this RUNME.rs is the
        // root of its own discovery).
        let entries = process_files_via_tree(&rnme_path, &[], &rnme_lib);
        let entry = &entries[0];

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
        generate_workspace(&cache_dir, &entries, &rnme_lib).unwrap();

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
    /// main.rs contains __rnme_link() calls for each crate, init hook
    /// collection/sorting logic, and registry building.
    #[test]
    fn test_integration_runner_main_content() {
        let rnme_lib = rnme_lib_path();

        let tmp = tempfile::TempDir::new().unwrap();

        // Create 3 RUNME.rs files
        let root_rnme = tmp.path().join("RUNME.rs");
        fs::write(&root_rnme, "fn deploy() {}\n").unwrap();

        let svc_dir = tmp.path().join("services").join("api");
        fs::create_dir_all(&svc_dir).unwrap();
        let svc_rnme = svc_dir.join("RUNME.rs");
        fs::write(&svc_rnme, "fn serve() {}\n").unwrap();

        let infra_dir = tmp.path().join("infra");
        fs::create_dir_all(&infra_dir).unwrap();
        let infra_rnme = infra_dir.join("RUNME.rs");
        fs::write(&infra_rnme, "fn provision() {}\n").unwrap();

        let entries = process_files_via_tree(
            &root_rnme,
            &[svc_rnme.clone(), infra_rnme.clone()],
            &rnme_lib,
        );
        {
        }

        // Generate workspace
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        generate_workspace(&cache_dir, &entries, &rnme_lib).unwrap();

        // Read the generated runner main.rs
        let runner_main = fs::read_to_string(cache_dir.join("runner/src/main.rs")).unwrap();

        // Verify __rnme_link() calls for each crate
        assert!(
            runner_main.contains("root::__rnme_link();"),
            "runner main should call root::__rnme_link(), got:\n{}",
            runner_main,
        );
        assert!(
            runner_main.contains("services_api::__rnme_link();"),
            "runner main should call services_api::__rnme_link(), got:\n{}",
            runner_main,
        );
        assert!(
            runner_main.contains("infra::__rnme_link();"),
            "runner main should call infra::__rnme_link(), got:\n{}",
            runner_main,
        );

        // Verify init hook collection from inventory
        assert!(
            runner_main.contains("rnme::inventory::iter::<rnme::init::InitDef>"),
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
            runner_main.contains("rnme::inventory::iter::<rnme::init::GroupDef>"),
            "runner main should collect GroupDefs from inventory, got:\n{}",
            runner_main,
        );

        // Verify registry building
        assert!(
            runner_main.contains("rnme::task::Registry::from_inventory()"),
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
            runner_main.contains("rnme::tokio::runtime::Builder::new_multi_thread()"),
            "runner main should build tokio runtime, got:\n{}",
            runner_main,
        );

        // Verify dispatch is handed off to cli::run()
        assert!(
            runner_main.contains("rnme::cli::run("),
            "runner main should hand off to rnme::cli::run(), got:\n{}",
            runner_main,
        );
    }
}
