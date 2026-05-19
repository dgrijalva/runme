# Proposal: `apply-rename` (revised)

**Task:** Phase 2 / `apply-rename` from `docs/plans/2026-05-18-typed-task-invocation.md`
**Author:** `impl-apply-rename`
**Status:** revised per `lead` policy answers — awaiting final sign-off

Wire `Frontmatter.rename` (parsed in Phase 1) into the three places that derive identifiers from a RUNME.rs's directory path: the cargo crate name, the inventory `group_key`, and the `__RNME_GROUP` constant injected by `transform_source`. A single helper resolves the path with the rename applied; all three call sites read from it so they cannot drift apart.

**Revisions in this version:**
- Root-file rename is rejected at workspace-generation time (was OPEN-1, now resolved).
- Rename values are pre-normalized with `heck::ToSnakeCase::to_snake_case` before substitution (was OPEN-2, now resolved).
- §5 OPEN section dropped — only OPEN-3 (helper placement) remained, and `lead` already approved it.

## 1. Helper signature

A private free function in `src/bin/rnme/compile.rs`, local to where `CrateEntry` is constructed:

```rust
/// Compute the effective relative directory path for a RUNME.rs,
/// applying `[rnme.rename]` if present.
///
/// The basename of the parent directory (the component immediately containing
/// the RUNME.rs file) is substituted with `heck::to_snake_case(rename)` when
/// `rename` is `Some`. The returned path is shaped exactly like
/// `rel_path.parent()` would be — same leading components, just with the
/// basename swapped.
///
/// Returns `Err` when `rename.is_some()` and `rel_path` is a root file (no
/// directory basename to substitute). The error is propagated up through
/// `process_rnme_file` so cargo build fails with a clear message.
fn resolved_dir(rel_path: &Path, rename: Option<&str>) -> Result<PathBuf, CompileError>;
```

**Fallback when rename is absent:** `resolved_dir(rel_path, None)` returns `Ok(rel_path.parent().unwrap_or(Path::new("")).to_path_buf())` — exactly what the existing code computes today. No behavior change for unrenamed files.

**What "basename" means here:** the last component of `rel_path.parent()`. For `services/auth/RUNME.rs`, `rel_path.parent()` is `services/auth`, basename `auth`. With `rename = Some("foo_bar_v2")`, the helper returns `services/foo_bar_v2`.

**Root-file detection:** the root file is the one where `rel_path.parent()` is empty / `.` / `""`. Concretely:

```rust
let parent = rel_path.parent().unwrap_or(Path::new(""));
let is_root = parent.as_os_str().is_empty() || parent == Path::new(".");
```

When `is_root && rename.is_some()`, return a `CompileError::RootRename` (new variant) carrying the file path and the offending rename value. Error message text:

> error: `[rnme.rename]` is not allowed on the root RUNME.rs (`<path>`); rename only applies to child RUNME.rs files because the root's own name is implicit and not part of its own import path.

A new `CompileError::RootRename(PathBuf, String)` variant goes in compile.rs's existing `CompileError` enum. Its `Display` impl produces the message above.

## 2. Substitution mechanics

Substitution happens once, in `process_rnme_file` (compile.rs, lines 116-186), *before* `crate_name_from_path` and the group-key derivation. Sketch:

```rust
fn process_rnme_file(file: &Path, root_dir: &Path, rnme_lib_path: &Path)
    -> Result<CrateEntry, CompileError>
{
    let source = fs::read_to_string(file).map_err(CompileError::ReadSource)?;
    let frontmatter = parse_frontmatter(&source);

    let rel_path = file.strip_prefix(root_dir).unwrap_or(file);

    // === single substitution point ===
    let effective_dir = resolved_dir(rel_path, frontmatter.rename.as_deref())?;
    let effective_rel = effective_dir.join("RUNME.rs");

    // (1) crate name — normalization runs over the substituted path
    let crate_name = crate_name_from_path(&effective_rel);

    // (2) group key — derived from the substituted dir
    let group_key = group_key_from_dir(&effective_dir);

    // (3) __RNME_GROUP — transform_source reads the same group_key
    let lib_source = transform_source(&source, &group_key);
    // ... (cargo_toml construction continues unchanged, using `crate_name`)
}
```

**Two-stage normalization for rename values.** `heck` is the rename-value pre-normalizer; the existing path-to-ident normalizer (`crate_name_from_path`) is unchanged and untouched for path-derived inputs. Pipeline for a rename value:

```rust
use heck::ToSnakeCase;

let basename_snake = rename.to_snake_case();
// substitute basename_snake into the path
```

The substituted path then passes through `crate_name_from_path` (which handles `/`, `-`, `.` → `_` and keyword/digit-prefix guards) exactly as it does today. For a value `"Hello World"`:

- heck: `"Hello World"` → `"hello_world"`
- substituted dir: `services/hello_world`
- `crate_name_from_path`: `services_hello_world` (slashes replaced)
- group key: `services/hello_world` (slashes preserved as the path-shape string)

For `"FooBar"`:

- heck: `"FooBar"` → `"foo_bar"`
- substituted dir: `foo/foo_bar`
- `crate_name_from_path`: `foo_foo_bar`
- group key: `foo/foo_bar`

For `"foo-bar"`:

- heck: `"foo-bar"` → `"foo_bar"` (heck collapses dashes to underscores in snake-case)
- substituted dir: `services/foo_bar`
- `crate_name_from_path`: `services_foo_bar`
- group key: `services/foo_bar`

heck is applied **only** to rename values. Path components that come from the filesystem are not run through heck — `crate_name_from_path` continues to handle them as it does today. This preserves all existing behavior for non-renamed files.

## 3. All three call-site changes

### (a) Cargo crate name — `Cargo.toml` emission

Currently `compile.rs:128`:
```rust
let crate_name = crate_name_from_path(rel_path);
```

After:
```rust
let crate_name = crate_name_from_path(&effective_rel);
```

The emitted `Cargo.toml` (`[package].name`, `[lib].name`), the workspace `members` array, the runner crate's `[dependencies]` table, and the `__rnme_link()` call site in `runner/main.rs` all key off the single `crate_name` field on `CrateEntry`. Nothing else needs to change — `crate_name` is already the single source of truth for "what cargo calls this crate".

### (b) `group_key` field on `CrateEntry`

Currently `compile.rs:131-146` computes `group_key` inline from `rel_path.parent()`. After this change, it computes from `effective_dir`. The logic for normalizing `"."` → `""` and stripping `"./"` / trailing `/` is preserved verbatim — extracted into a small helper `group_key_from_dir(&Path) -> String` so the substitution site can call it. The helper is a straight refactor of the current inline code; no semantic change.

### (c) `__RNME_GROUP` injection via `transform_source`

`transform_source(&source, &group_key)` already takes the group string as a parameter (compile.rs:149, transform.rs:8). No signature change. The change is purely that `group_key` is now derived from `effective_dir`, so the injected `const __RNME_GROUP: &str = "...";` reflects the renamed path.

## 4. Cargo.toml change

Add `heck` to `[dependencies]` in the root `rnme` crate's `Cargo.toml`. The CLI binary lives under `src/bin/rnme/` and is part of the `rnme` crate, so `[dependencies]` is the right home (no separate `[bin-dependencies]` in cargo).

```toml
heck = "0.5"
```

Latest stable is 0.5.x.

## 5. Test plan

All new tests go in the existing `#[cfg(test)] mod tests` block in `compile.rs`. Style mirrors the existing `test_process_rnme_file*` tests.

### Unit tests on `resolved_dir`

| Test name | Input | Asserts |
|---|---|---|
| `test_resolved_dir_no_rename` | `("foo/bar/RUNME.rs", None)` | `Ok(PathBuf::from("foo/bar"))` |
| `test_resolved_dir_with_rename` | `("foo/bar/RUNME.rs", Some("baz"))` | `Ok(PathBuf::from("foo/baz"))` |
| `test_resolved_dir_rename_snake_cases` | `("foo/bar/RUNME.rs", Some("Hello World"))` | `Ok(PathBuf::from("foo/hello_world"))` — heck applied |
| `test_resolved_dir_rename_camel_case` | `("foo/bar/RUNME.rs", Some("FooBar"))` | `Ok(PathBuf::from("foo/foo_bar"))` |
| `test_resolved_dir_rename_dashes` | `("foo/bar/RUNME.rs", Some("foo-bar-v2"))` | `Ok(PathBuf::from("foo/foo_bar_v2"))` |
| `test_resolved_dir_root_no_rename` | `("RUNME.rs", None)` | `Ok(PathBuf::from(""))` |
| `test_resolved_dir_root_with_rename_errors` | `("RUNME.rs", Some("anything"))` | `Err(CompileError::RootRename { ... })` |
| `test_resolved_dir_root_dotslash_with_rename_errors` | `("./RUNME.rs", Some("x"))` | `Err(CompileError::RootRename { ... })` |

### End-to-end tests on `process_rnme_file`

| Test name | Scenario | Asserts |
|---|---|---|
| `test_process_rnme_file_with_rename` | `foo/RUNME.rs` with `//! [rnme.rename]\n//! name = "foo_bar_v2"` | `entry.crate_name == "foo_bar_v2"`; `entry.group_key == "foo_bar_v2"`; `entry.lib_source` contains `const __RNME_GROUP: &str = "foo_bar_v2";`; `entry.cargo_toml` contains `name = "foo_bar_v2"` |
| `test_process_rnme_file_with_rename_nested` | `services/auth/RUNME.rs` renamed to `"auth_v2"` | `crate_name == "services_auth_v2"`; `group_key == "services/auth_v2"`; `__RNME_GROUP == "services/auth_v2"` |
| `test_process_rnme_file_rename_heck_normalizes_hello_world` | `foo/RUNME.rs` renamed to `"Hello World"` | `crate_name == "hello_world"`; `group_key == "hello_world"`; both `Cargo.toml` and `__RNME_GROUP` agree |
| `test_process_rnme_file_rename_heck_camel_case` | `foo/RUNME.rs` renamed to `"FooBar"` | `crate_name == "foo_bar"`; `group_key == "foo_bar"` |
| `test_process_rnme_file_rename_absent_unchanged` | `foo/RUNME.rs` with `[rnme.rename]` section but no `name = ...` line (yields `rename: None`) | identical to a file with no rename block — `crate_name == "foo"`, `group_key == "foo"` |
| `test_process_rnme_file_root_rename_errors` | root `RUNME.rs` with `[rnme.rename] name = "x"` | `process_rnme_file(...)` returns `Err(CompileError::RootRename(...))` with the file path and `"x"` carried |

### Non-rename regression test

| Test name | Scenario | Asserts |
|---|---|---|
| `test_path_normalization_unchanged_for_non_renamed_unicode` | `café/RUNME.rs` with no rename | `crate_name == "café"` (existing `test_unicode_in_path` behavior preserved). Confirms heck is NOT applied to path-derived inputs |

### Dynamic-path agreement

Per `lead`'s approval, `ctx.run("foo_bar_v2:task", &[])` runtime resolution is validated by `val-rename`, not in this task. This task's contract is three-way identifier agreement, which is asserted structurally in the `test_process_rnme_file_with_rename*` cases above (crate name, group key, and `__RNME_GROUP` all derived from `effective_dir`).

## 6. Decisions to confirm (revised)

1. Free-fn `resolved_dir(&Path, Option<&str>) -> Result<PathBuf, CompileError>` in `compile.rs`, called once in `process_rnme_file`. **Returns `Err(CompileError::RootRename(...))` when rename is set on the root file.** (§1, §2)
2. New `CompileError::RootRename(PathBuf, String)` variant with `Display` impl per §1.
3. `heck = "0.5"` added to `[dependencies]` in the root `Cargo.toml`; `heck::ToSnakeCase::to_snake_case` applied to rename values inside `resolved_dir`. (§2, §4)
4. Three call-site changes route through the single resolved `effective_dir`. (§3)
5. `group_key_from_dir(&Path) -> String` helper extracted from the existing inline code; no semantic change.
6. Test plan as listed in §5; runtime dispatch end-to-end check deferred to `val-rename`.

Awaiting `lead` final sign-off before implementing.

---

# Revision 2 — tree-traversal restructure

**Status:** revised per `team-lead` direction (user-driven structural change) — awaiting `lead` review.

## Why Revision 2

Revision 1 landed and passed tests, but the user reviewed it and surfaced a structural issue: `resolved_dir` was called on every entry — root included — and used an `is_root` runtime branch to reject root rename. The constraint "rename only applies to children" should be enforced by **where the code runs**, not by a runtime check.

The replacement model: rename application happens **only inside a child-iteration loop**. Root is the entry point to the traversal and is never visited as a child of any other node, so its frontmatter rename is structurally inaccessible. There is no `is_root` branch because there is no code path on which a root is mistaken for a child.

This is also the data structure `subtasks-injection` (#14) will walk to emit `mod subtasks { pub mod <child_module_name> { ... } }`. The helper this task introduces (`normalize_module_name`) is the same one #14 will call per child.

## What's removed from Revision 1

- `resolved_dir(&Path, Option<&str>, &Path) -> Result<PathBuf, CompileError>` — gone.
- `CompileError::RootRename(PathBuf, String)` variant + its `Display` arm — gone (no code path produces it).
- Tests: `test_resolved_dir_*` (all 9 of them), `test_process_rnme_file_root_rename_errors`, and `test_process_rnme_file_rename_absent_unchanged` — all gone (the underlying helper is removed; the rename-absent case is covered by the new tree-traversal tests below).
- `heck` import in `compile.rs` moves with the helper into wherever `normalize_module_name` ends up living.

## What's added

### 1. Node type

A simple tree node carrying:

```rust
struct ModuleNode {
    /// Path to the RUNME.rs source file (absolute or as-discovered).
    path: PathBuf,
    /// Already-resolved module/crate name. Root: caller-assigned ("root").
    /// Children: result of `normalize_module_name(child_path, &renames)`.
    module_name: String,
    /// The resolved relative directory from `root_dir` (used to derive
    /// `group_key` and to build the `effective_rel` for cargo path).
    /// Root: "" (or "."). Children: parent_effective_dir.join(module_name).
    effective_dir: PathBuf,
    /// Parsed frontmatter for this node — kept here so workspace generation
    /// reads deps without re-parsing. For root, `frontmatter.rename` is
    /// ignored (never read by any consumer).
    frontmatter: Frontmatter,
    /// Source text (read once, reused for transform_source + frontmatter).
    source: String,
    /// Recursive child nodes. Empty for leaves.
    children: Vec<ModuleNode>,
}
```

Implementation note: `source` and `frontmatter` are stored on the node because (a) we already read the file to parse the frontmatter, and (b) workspace generation needs the source again for `transform_source`. Keeping them on the node avoids re-reading or re-parsing.

### 2. `normalize_module_name` helper

Replaces the rename-application part of `resolved_dir`. Lives next to `crate_name_from_path` in `crate_name.rs` (since it's a path-to-ident-shape helper), or co-located with the tree-build code in `compile.rs` if that reads better. Proposing `crate_name.rs` because (a) it's the same family of operation, (b) `subtasks-injection` will import it from there.

```rust
/// Compute the module/crate name for a child RUNME.rs entry, applying
/// `[rnme.rename]` from the renames map if present.
///
/// Pipeline:
///   1. If `renames.get(child_path)` is `Some(name)`, use
///      `heck::to_snake_case(name)` as the basename.
///      Else, use the directory basename of `child_path.parent()`.
///   2. Build a relative path with the basename swapped in.
///   3. Pass to `crate_name_from_path` for final normalization
///      (`/`, `-`, `.` → `_`, keyword/digit guards).
///
/// This is **only ever called for child entries**. Root entries are
/// assigned "root" directly by the caller; this function is never
/// invoked with root's path. There is no `is_root` branch.
pub fn normalize_module_name(
    child_rel_path: &Path,
    renames: &HashMap<PathBuf, String>,
) -> String;
```

**Returns the resolved crate/module name only.** Group key is a separate derivation handled at the tree-assembly site (effectively `parent_effective_dir.join(basename)` — see §3 below).

### 3. `build_module_tree` — child-iteration only

Top-level function that takes the discovery result and produces a tree rooted at root. Sketch:

```rust
fn build_module_tree(
    discovery: &DiscoveryResult,
    root_dir: &Path,
) -> Result<ModuleNode, CompileError>;
```

Behavior:

1. **Root construction** (outside any iteration loop):
   - Read `root_rnme` source.
   - Parse its frontmatter.
   - Build a root `ModuleNode` with `module_name = "root"`, `effective_dir = ""` (root group), regardless of what root's `frontmatter.rename` says. Root's `rename` is **not consulted**.
2. **Build the renames map**: walk `discovery.children`, for each child whose frontmatter has `rename: Some(_)`, record `child_path → rename_value` in a `HashMap<PathBuf, String>`. (Root is not in this map.)
3. **Assemble parent→children edges** by directory ancestry. The simplest implementation:
   - For each child, compute the parent RUNME.rs file by walking up from `child.parent()` looking for the deepest ancestor in the discovery set (falls back to root).
   - Attach each child to its parent's `children` Vec.
4. **Inside the child-iteration loop only**: call `normalize_module_name(child_rel_path, &renames)` to get the child's `module_name`. The child's `effective_dir` is `parent.effective_dir.join(module_name)`.

**Critical property:** root's frontmatter rename is never read by any code path inside `build_module_tree` (or anywhere else). There is no need to check or reject it — it's simply ignored by virtue of not being in the renames map and not being reachable as a child.

OPEN (tactical, lead-decides): step 3's parent attachment is the part most likely to be implementation-shaped vs. structural. I propose a simple two-pass approach: pass 1 builds a path→`ModuleNode` map keyed by `child.parent()`; pass 2 attaches each non-root node to its parent. If `lead` prefers a recursive build-as-you-go approach (matching the user's pseudocode literally), I'll do that instead — both produce identical trees.

### 4. Workspace generation reads from the tree

`compile_workspace`:
- Calls `build_module_tree(discovery, root_dir)` once.
- Flattens the tree (depth-first) into the existing `Vec<CrateEntry>` for the generators that already exist (`generate_workspace`, `generate_runner_main`). The flattening step builds each `CrateEntry` from its corresponding `ModuleNode` — `crate_name = node.module_name`, `group_key = group_key_from_dir(&node.effective_dir)`, `lib_source = transform_source(&node.source, &group_key)`, deps from `node.frontmatter`.

`process_rnme_file` either disappears or becomes a small `node → CrateEntry` projection. Net: the file-system → string-name resolution moves from `process_rnme_file` (per-entry, with `is_root`) into `build_module_tree` (whole-tree, child-only).

### 5. What stays the same from Revision 1

- `group_key_from_dir(&Path) -> String` helper. Still used; called from the tree-flattening step.
- `heck = "0.5"` dependency. Still used inside `normalize_module_name`.
- `crate_name_from_path` untouched for path-derived inputs (still the final normalizer over the substituted path).
- The "three callsites read from a single source of truth" property — now the source of truth is `ModuleNode`, not a per-entry helper.

## Revised test plan

All tests live in `compile.rs` (or `crate_name.rs` for the `normalize_module_name` unit tests, depending on final placement).

### `normalize_module_name` unit tests

| Test name | Inputs | Asserts |
|---|---|---|
| `test_normalize_no_rename` | `("foo/bar/RUNME.rs", empty map)` | returns `"foo_bar"` (existing path normalization) |
| `test_normalize_with_rename` | `("foo/bar/RUNME.rs", {<that path> → "baz"})` | returns `"foo_baz"` |
| `test_normalize_rename_snake_cases_hello_world` | rename = `"Hello World"` on a single-level path | returns `"hello_world"` |
| `test_normalize_rename_camel_case` | rename = `"FooBar"` | returns `"foo_bar"` |
| `test_normalize_rename_dashes` | rename = `"foo-bar-v2"` | returns `"foo_bar_v2"` |
| `test_normalize_rename_does_not_apply_to_unmapped_path` | renames map has an unrelated entry | path normalization runs unchanged |

### Tree-traversal end-to-end tests

| Test name | Scenario | Asserts |
|---|---|---|
| `test_build_module_tree_root_only` | Single root RUNME.rs, no children, no rename | tree has one node with `module_name == "root"`, `effective_dir == ""`, no children |
| `test_build_module_tree_root_with_rename_is_ignored` | Root has `[rnme.rename] name = "monorepo"`; no children | root node's `module_name` is still `"root"` (rename was never consulted). No error. |
| `test_build_module_tree_child_with_rename` | Root + `foo/RUNME.rs` with rename `"foo_bar_v2"` | tree root has one child with `module_name == "foo_bar_v2"`, `effective_dir == "foo_bar_v2"` |
| `test_build_module_tree_nested_child_with_rename` | Root + `services/auth/RUNME.rs` with rename `"auth_v2"` | (depending on parent attachment) child's `module_name` reflects rename + `services_` prefix from path; `effective_dir == "services/auth_v2"` (or sibling node `services` plus child `auth_v2` — finalize during impl) |
| `test_build_module_tree_child_rename_heck_normalizes` | child with rename `"Hello World"` | child's `module_name == "hello_world"` |
| `test_build_module_tree_unicode_path_unchanged` | child at `café/RUNME.rs` with no rename | child's `module_name == "café"` (heck NOT applied) |

### Three-way agreement at the workspace level

Tests that take a `ModuleNode` (or run the full `compile_workspace`-equivalent flatten step) and assert that for a renamed child, all three of: cargo `[package].name`, the `__RNME_GROUP` constant, and the workspace `members` entry agree on the resolved name. This carries forward the existing `test_process_rnme_file_with_rename*` style at the new layer.

| Test name | Asserts |
|---|---|
| `test_workspace_emits_renamed_child` | child `foo/RUNME.rs` with rename `"foo_bar_v2"` → emitted `CrateEntry.crate_name == "foo_bar_v2"`; `lib_source` contains `const __RNME_GROUP: &str = "foo_bar_v2";`; `cargo_toml` contains `name = "foo_bar_v2"` |
| `test_workspace_renamed_nested` | `services/auth/RUNME.rs` rename `"auth_v2"` → matching three-way agreement |
| `test_workspace_root_rename_observably_noop` | root with `[rnme.rename] name = "ignored"` → emitted root `CrateEntry.crate_name == "root"`, `__RNME_GROUP == ""` (rename silently ignored, no error) |

## Coordination with `impl-fixture`

The fixture-side `root_rename_is_rejected` test in `tests/typed_invocation.rs` (plus the `ROOT_RENAME_FIXTURE` LazyLock) will break once `RootRename` is gone. Per `team-lead`'s direction: I will flip that one test to `#[ignore]` with a TODO comment naming `impl-fixture` as the next owner. I will NOT delete the fixture directory under `testing/fixtures/typed_invocation_root_rename/` — that's `impl-fixture`'s call.

## Decisions to confirm (Rev 2)

1. `ModuleNode` shape as sketched in §1, including holding `source` and `frontmatter` on the node so workspace generation doesn't re-read. (§1)
2. `normalize_module_name` lives in `crate_name.rs` (next to `crate_name_from_path`); takes `(&Path, &HashMap<PathBuf, String>) -> String`. (§2)
3. `build_module_tree(&DiscoveryResult, &Path) -> Result<ModuleNode, CompileError>` produces a recursive tree; root assigned `"root"` outside any loop; rename application only inside child iteration. (§3)
4. Workspace generation flattens the tree → `Vec<CrateEntry>` and feeds the existing generators. `process_rnme_file` either becomes a thin `node → CrateEntry` projection or is removed entirely (lead's call — both yield the same output).
5. Parent attachment strategy in step 3: two-pass (build map, then attach) vs. recursive walk that mirrors the user's pseudocode literally. (Tactical; either works.)
6. `root_rename_is_rejected` test in `tests/typed_invocation.rs` flipped to `#[ignore]` with TODO; fixture dir untouched.
7. Tests as listed; revision 1 tests for `resolved_dir` and `RootRename` removed.

Awaiting `lead` review.

