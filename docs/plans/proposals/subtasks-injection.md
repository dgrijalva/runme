# Subtasks Injection — Proposal

**Task:** `subtasks-injection` (Phase 3, plan §319)
**Author:** `impl-subtasks-injection`
**Status:** awaiting review
**Depends on:** `typed-shim-macro` (G2), `apply-rename` (landed)

This proposal commits to: (a) the algorithm that walks the existing `ModuleNode` tree and produces per-parent `mod subtasks { ... }` blocks (including the structural-only intermediate dir case), (b) the exact emitted Rust source per node, (c) the integration point in `compile.rs`, (d) how parent Cargo.toml grows transitive path-deps on every descendant in its subtree, (e) the acyclicity assertion, and (f) the test plan.

Nothing in this proposal touches `macros/`, the engine, or the fixture. All changes are localized to `src/bin/rnme/compile.rs` (with optional small helper extraction). `ModuleNode` is reused as-is; no new fields are required.

---

## 1. Inputs and reuse

`build_module_tree` (compile.rs:151) already returns a `ModuleNode` with:

- `path: PathBuf` — the source RUNME.rs.
- `module_name: String` — already-resolved identifier (e.g. `"api_v2"` for a renamed dir, `"services"` for an unrenamed one, `"root"` for the root).
- `effective_dir: PathBuf` — relative-from-root with renames substituted (e.g. `"services/api_v2"`).
- `children: Vec<ModuleNode>` — only children that actually have a RUNME.rs file.

Crucially: **`effective_dir` is the renamed-path-to-this-node**, so a child's `effective_dir` already encodes any rename of any ancestor along the way (in practice only its immediate parent, because rename only swaps the basename). That means the *segments between* a parent's `effective_dir` and a child's `effective_dir`, after stripping the parent's prefix, are exactly the on-disk subpath from parent to child — including any intermediate dirs that lack a RUNME.rs.

That subpath is the data I need to detect structural-only intermediates. No new fields on `ModuleNode`; no new helpers in `crate_name.rs`.

**`module_name` is the identifier to use in `pub mod <name>`.** It's already keyword-/digit-guarded and snake-cased.

---

## 2. Walking algorithm

Per parent node `P` (root and every real descendant), build the parent's `subtasks` block from `P.children` recursively. For each child `C`:

1. Compute the path segment from `P.effective_dir` to `C.effective_dir`:
   ```
   segment = C.effective_dir.strip_prefix(&P.effective_dir)
   ```
   This yields a relative path like `"api_v2"` (direct child), or `"structural_only/leaf"` (child of a structural-only intermediate dir), or `"a/b/c/d"` (multiple structural-only intermediates stacked).

2. Walk `segment`'s components left-to-right. For every component **except the last**, emit a `pub mod <component> { ... }` with no `pub use` inside it (structural-only intermediate). For the **last** component, emit:
   ```rust
   pub mod <C.module_name> {
       pub use ::<C.crate_name>::*;
       // ...then recurse into C's own subtasks (children of C):
   }
   ```

   The last-component identifier comes from `C.module_name`, not from the path segment — this is the only difference between intermediate components and the terminal component, and it's what makes a rename like `api → api_v2` land on the `api_v2` ident at the leaf while leaving everything above untouched.

3. Inside the terminal `pub mod`, recurse into `C.children` using the same algorithm with `C` as the new parent. Recursion is bounded by the tree depth and visits each tree node once.

**Why this is correct for renames.** A renamed child's `effective_dir` already has the substituted basename in its final segment (proven by `test_build_module_tree_child_with_rename` in compile.rs:683). So:

- For `services/api/RUNME.rs` renamed to `api_v2` (under `services/RUNME.rs`):
  - `P = services`, `C = api_v2`
  - `segment = strip_prefix("services/api_v2", "services") = "api_v2"`
  - Components: `["api_v2"]`. Single component → emit terminal `pub mod api_v2 { pub use ::services_api_v2::*; }`.
- For the same fixture but with no intermediate `services/RUNME.rs`:
  - `P = root`, `C = api_v2`
  - `segment = "services/api_v2"`. Components: `["services", "api_v2"]`.
  - Emit `pub mod services { pub mod api_v2 { pub use ::services_api_v2::*; } }` — `services` is structural-only.

**Why this is correct for structural-only intermediates with no RUNME.rs.** The fixture has `structural_only/leaf/RUNME.rs` and no `structural_only/RUNME.rs`. The tree's root has `leaf` as a child with `effective_dir = "structural_only/leaf"`. Per step 1, `segment = "structural_only/leaf"`, components `["structural_only", "leaf"]`. Per step 2, `structural_only` is structural-only (no `pub use`), `leaf` is terminal. Emission:
```rust
pub mod structural_only {
    pub mod leaf {
        pub use ::structural_only_leaf::*;
    }
}
```

**Sibling structural-only paths converge.** If two children share a structural-only prefix (e.g. `a/b/c/RUNME.rs` and `a/b/d/RUNME.rs`), naive iteration would emit `pub mod a { pub mod b { ... } }` twice. The implementation merges siblings into a single structural module at codegen time using an intermediate accumulator data structure (sketch in §3.3 below). This is purely codegen-side and doesn't touch `ModuleNode`.

---

## 3. Emission shape

### 3.1 Per-parent block placement

For every node `P` (root + every descendant that itself has children, recursively), append to `P`'s `lib_source`:

```rust

// === subtasks (auto-generated) ===
#[allow(unused_imports, dead_code)]
mod subtasks {
    <recursive body>
}
```

`mod subtasks` is **not** `pub` (design doc §3: "not pub — local to this crate"). The block is wrapped in `#[allow(unused_imports, dead_code)]` because:
- `pub use ::child_crate::*` re-exports everything the child marked `pub`, much of which the parent will never reference.
- `pub mod <child>` for a child whose tasks the parent doesn't invoke would otherwise warn.

If `P` has no children at all, emit nothing (per the plan brief: "Don't emit anything if a parent has no descendants").

### 3.2 Recursive body

The recursive body uses the algorithm from §2. Pseudocode for the emission, with the merging behavior:

```
fn emit_subtasks_body(parent: &ModuleNode) -> String:
    // Build a "merge tree": a nested map keyed by path components.
    // Leaves carry the terminal ModuleNode (so we know the renamed
    // module_name and crate_name); internal nodes are structural-only.
    let mut merge = MergeNode::new_intermediate();
    for child in &parent.children:
        let segment = child.effective_dir.strip_prefix(&parent.effective_dir);
        merge.insert_descendant(segment, child);
    render(&merge, 0)
```

`MergeNode` is a tiny internal type with two variants:

```rust
enum MergeNode<'a> {
    /// Structural-only directory. Children keyed by *raw path segment string*
    /// (since structural dirs have no rename and no `module_name`).
    Intermediate { children: BTreeMap<String, MergeNode<'a>> },
    /// A real descendant — carries a reference to the `ModuleNode` so we
    /// can read its `module_name`, `crate_name`, and recurse into its own
    /// `children`.
    Terminal { node: &'a ModuleNode },
}
```

Rendering rules:

- `Intermediate { children }`: for each `(key, child)` in `children`, emit `pub mod <key> { <render(child)> }`. `key` is already a valid identifier for structural dirs (no RUNME.rs means no rename to apply, and the path component itself goes through whatever normalization the design implies — see open question #1 below).
- `Terminal { node }`: emit `pub mod <node.module_name> { pub use ::<node.crate_name>::*; <emit_subtasks_body(node)> }`. The recursive call inside the terminal emits the *grandchildren's* subtasks tree.

`BTreeMap` keys give deterministic ordering for stable codegen output (test-friendliness).

### 3.3 Worked 3-level example

Given (from the existing fixture plus a hypothetical `services/RUNME.rs` parent):

```
testing/fixtures/typed_invocation/
├── RUNME.rs                              (root)
├── HelloWorld/RUNME.rs                   (renamed → hello_world)
├── child_a/RUNME.rs
├── services/RUNME.rs                     (intermediate, has its own tasks)
│   └── api/RUNME.rs                      (renamed → api_v2)
└── structural_only/
    └── leaf/RUNME.rs                     (no structural_only/RUNME.rs)
```

The root crate's emitted lib_source ends with:

```rust

// === subtasks (auto-generated) ===
#[allow(unused_imports, dead_code)]
mod subtasks {
    pub mod child_a {
        pub use ::child_a::*;
    }
    pub mod hello_world {
        pub use ::hello_world::*;
    }
    pub mod services {
        pub use ::services::*;
        pub mod api_v2 {
            pub use ::services_api_v2::*;
        }
    }
    pub mod structural_only {
        pub mod leaf {
            pub use ::structural_only_leaf::*;
        }
    }
}
```

The `services` crate's emitted `lib_source` ends with its own `mod subtasks` containing only what's in *its* subtree:

```rust

// === subtasks (auto-generated) ===
#[allow(unused_imports, dead_code)]
mod subtasks {
    pub mod api_v2 {
        pub use ::services_api_v2::*;
    }
}
```

The `services_api_v2`, `child_a`, `hello_world`, and `structural_only_leaf` crates have no children, so their `lib_source` gets no `mod subtasks` block at all (per plan brief).

### 3.4 What `module_name` is for an intermediate dir

`module_name` only exists on `ModuleNode`s that have a RUNME.rs. For structural-only intermediates, the merge tree uses the **raw path component string** from `effective_dir`. Because rename only swaps the *final* component (the dir containing the RUNME.rs), all intermediate components are the on-disk dir names — they were never renamed by anyone. They flow through `effective_dir` verbatim.

**Open question 1 (escalation candidate):** structural-only dir names are not run through any normalizer. If someone has a directory named `mod` or `3rdparty` or `foo-bar` *on the path to* a descendant RUNME.rs, the emitted `pub mod mod { ... }` / `pub mod 3rdparty { ... }` / `pub mod foo-bar { ... }` would be a Rust syntax error. I see three options and want lead's call:

1. **Run intermediate components through `crate_name_from_path` segment-by-segment.** Same normalizer the renamed/terminal names use; produces valid idents. Cost: an unrenamed `foo-bar/` intermediate becomes `foo_bar` in the subtasks path, which could surprise authors (their dir is `foo-bar` on disk, but the path they write is `subtasks::foo_bar::...`). This is what I'd propose if forced — it matches how renamed/terminal names work.
2. **Error at codegen** if any intermediate component isn't a valid Rust ident. Loud, narrow, easy to recover from by adding a RUNME.rs at that level or renaming the dir.
3. **Leave the strings verbatim and let cargo throw a syntax error.** Worst UX; not seriously proposing it.

I am going to implement option (1) unless lead says otherwise, because it's the closest match to how `module_name` is already computed for nodes that have a RUNME.rs. The fixture doesn't currently exercise this case (`structural_only` is a clean ident), so the choice has no acceptance-test impact for this task — but it's a real edge that wants a decision.

---

## 4. Where the emission lands in the pipeline

Per the plan brief and the task summary: append to `lib_source` inside `compile.rs`, **not** in `transform.rs`. Concretely:

`transform_source(&node.source, &group_key)` currently produces:
```
const __RNME_GROUP: &str = "<group>";
<stripped source>
pub fn __rnme_link() {}
```

The new logic, executed in `node_to_crate_entry` (compile.rs:302) after `transform_source` returns:

```rust
let mut lib_source = transform_source(&node.source, &group_key);
if !node.children.is_empty() {
    let subtasks_block = emit_subtasks_block(node);
    lib_source.push_str(&subtasks_block);
}
```

`emit_subtasks_block(&ModuleNode) -> String` is the new entry point that:
1. Returns empty string if `node.children.is_empty()`.
2. Builds the merge tree and renders it inside the wrapping `mod subtasks { ... }`.

`transform_source` stays file-local to `transform.rs` and gets no new knobs. This matches the plan's explicit direction ("Keep `transform_source` file-local; do the appending in `compile.rs`").

The recursive descent into descendants happens *implicitly* via `flatten_tree_into_entries` (compile.rs:341), which already visits every node depth-first. Because `emit_subtasks_block` runs inside `node_to_crate_entry`, every node — root, intermediate, descendant — gets its own correctly-scoped subtasks block.

---

## 5. Cargo.toml extension

Each parent crate's Cargo.toml needs path deps on every descendant crate (transitive — not just direct children, per plan §5).

The current `node_to_crate_entry` builds the Cargo.toml via string templating (compile.rs:310-336). I'll extend that by:

1. Collecting `descendant_crate_names: Vec<String>` for the node by walking `node.children` recursively and pulling each visited node's `module_name` (which equals the cargo crate name).
2. Appending a `<name> = { path = "../<name>" }` line per descendant *after* the existing user-frontmatter deps.

Sketch (replacement for the closing part of `node_to_crate_entry`):

```rust
for (name, version_spec) in &rewritten_deps {
    cargo_toml.push_str(&format!("{} = {}\n", name, version_spec));
}
// NEW: transitive descendant path-deps for the subtasks module tree.
let descendants = collect_descendant_crate_names(node);
for dep_name in &descendants {
    cargo_toml.push_str(&format!(
        "{} = {{ path = \"../{}\" }}\n",
        dep_name, dep_name,
    ));
}
```

`collect_descendant_crate_names(&ModuleNode) -> Vec<String>` is a new free fn (3 lines) that does a depth-first walk of `node.children` and pushes each visited node's `module_name`.

**Mechanics: string templating, not structured TOML.** The existing code already builds Cargo.toml by pushing format strings. I'll match that style — adding a `toml_edit` or `toml` dependency just to append four lines feels heavy. The format is identical to the runner's existing emission (compile.rs:402-407: `runner_cargo.push_str(&format!("{} = {{ path = \"../{}\" }}\n", ...))`), which is the precedent.

**Workspace `[dependencies]` section ordering.** The user-defined deps from frontmatter come first; the auto-generated path-deps follow. Since cargo dep order doesn't matter semantically, this is purely about readability. The auto-injected block is delimited by a leading comment line for human readers:

```
[dependencies]
rnme = { path = "..." }
tools = { path = "..." }              # from user frontmatter

# --- subtasks (auto-generated) ---
services_api_v2 = { path = "../services_api_v2" }
child_a = { path = "../child_a" }
```

The comment is decorative — cargo ignores it. Tests will assert the lines exist regardless of position.

---

## 6. Cycle guard

Per plan §5: "Add a debug_assert that the dep graph is acyclic (it must be — parent depends on descendants only)."

The dep graph **is structurally acyclic by construction**: parent crates depend only on crates whose `path` is in their subtree, and the subtree relation is itself a partial order (rooted DAG, actually a tree). So a cycle would require either (a) a bug in `collect_descendant_crate_names` or `build_module_tree` that puts an ancestor in a descendant's subtree, or (b) two distinct paths producing the same `module_name` (a sibling normalization collision — handled separately by `collision-detection`, task #17).

I'll place the assertion at the end of `flatten_tree_into_entries`, after all entries are built:

```rust
debug_assert!(
    subtasks_dep_graph_is_acyclic(&entries),
    "subtasks injection produced a cyclic dep graph"
);
```

`subtasks_dep_graph_is_acyclic(&[CrateEntry]) -> bool` is a small helper:

1. Build a map `crate_name → Vec<descendant_crate_name>` by parsing the auto-generated dep lines back out of each entry's `cargo_toml` (or — better — extend `CrateEntry` with `descendant_crate_names: Vec<String>` so the check doesn't depend on string parsing). I prefer the latter; it makes the check trivial and also makes the value testable directly without re-parsing.
2. Topologically sort; fail if any node remains.

**Open question 2:** add a `descendant_crate_names: Vec<String>` field to `CrateEntry`, or parse back from `cargo_toml`? My read: add the field. It's already used to build the cargo.toml and the cycle check; surfacing it as a structured field (a) makes tests trivial (no string parsing), (b) avoids fragility if the cargo.toml line format ever changes, and (c) costs ~5 bytes of struct overhead. I'll do this unless lead pushes back.

For Cargo.toml, this means `node_to_crate_entry` first computes the `Vec<String>`, then both writes the lines and stashes the Vec on the entry. The check reads from the field.

`debug_assert!` is debug-only by design (per plan brief). Release builds skip it, and that's fine — the structural argument above guarantees acyclicity unless a much earlier invariant is already broken.

---

## 7. Test plan

All new tests live in `compile.rs`'s existing `#[cfg(test)] mod tests` block. They exercise the codegen layer directly against synthetic `ModuleNode` trees (no temp dirs, no file I/O), plus one end-to-end test that runs the full pipeline.

Fixture changes are owned by `impl-fixture` (per the team-lead brief: "impl-fixture handles the flip; you don't touch fixture tests directly"). I will *not* edit `tests/typed_invocation.rs` or any file under `testing/fixtures/`.

### 7.1 Emission unit tests (no I/O)

| Test name | Synthetic tree | Asserts |
|---|---|---|
| `test_subtasks_empty_for_leaf` | root only (no children) | `emit_subtasks_block(&root)` returns empty string |
| `test_subtasks_single_child` | root + `foo/RUNME.rs` | emitted block contains `mod subtasks {`, `pub mod foo {`, `pub use ::foo::*;` |
| `test_subtasks_nested_intermediate_with_rnme` | root + `services/RUNME.rs` + `services/api/RUNME.rs` (renamed `api_v2`) | root's emission has `pub mod services { pub use ::services::*; pub mod api_v2 { pub use ::services_api_v2::*; } }`; services' emission has only `pub mod api_v2 { pub use ::services_api_v2::*; }` |
| `test_subtasks_structural_only_intermediate` | root + `structural_only/leaf/RUNME.rs` (no `structural_only/RUNME.rs`) | root's emission has `pub mod structural_only { pub mod leaf { pub use ::structural_only_leaf::*; } }` — `structural_only` has NO `pub use` |
| `test_subtasks_sibling_structural_paths_merge` | root + `a/b/c/RUNME.rs` + `a/b/d/RUNME.rs` (no `a/RUNME.rs`, no `a/b/RUNME.rs`) | root's emission has a single `pub mod a { pub mod b { pub mod c { ... } pub mod d { ... } } }` block (not two duplicate `pub mod a` siblings) |
| `test_subtasks_renamed_basename_emits_renamed_ident` | root + `foo/RUNME.rs` renamed `bar` | root's emission has `pub mod bar { pub use ::bar::*; }` (no mention of `foo`) |
| `test_subtasks_block_is_not_pub` | any tree with children | emitted text contains `mod subtasks {`, never `pub mod subtasks {` |
| `test_subtasks_not_emitted_for_no_children` | root + 1 leaf child | leaf's emitted lib_source contains no `mod subtasks` at all |
| `test_subtasks_deterministic_ordering` | root + several siblings inserted in different orders | output string identical regardless of `ModuleNode` Vec ordering (BTreeMap pin) |

### 7.2 Cargo.toml extension tests

| Test name | Tree | Asserts |
|---|---|---|
| `test_cargo_toml_descendant_path_deps_direct` | root + `foo/RUNME.rs` | root's `cargo_toml` contains `foo = { path = "../foo" }` |
| `test_cargo_toml_descendant_path_deps_transitive` | root + `services/RUNME.rs` + `services/api/RUNME.rs` | root's `cargo_toml` contains both `services = ...` and `services_api = ...`; services' `cargo_toml` contains `services_api = ...` (not the root!) |
| `test_cargo_toml_no_deps_for_leaf` | leaf node | no auto-injected `path` dep lines beyond the existing user-frontmatter ones |
| `test_cargo_toml_renamed_descendant_uses_renamed_crate_name` | root + `foo/RUNME.rs` renamed `bar` | root's `cargo_toml` contains `bar = { path = "../bar" }`, never `foo = ...` |

### 7.3 Cycle guard

| Test name | Asserts |
|---|---|
| `test_subtasks_dep_graph_acyclic_on_valid_tree` | `subtasks_dep_graph_is_acyclic` returns `true` for the synthetic 3-level tree from §3.3 |
| `test_subtasks_dep_graph_detects_cycle` | hand-construct a `Vec<CrateEntry>` with a forged cycle (e.g. parent lists itself as descendant); check returns `false` |

### 7.4 End-to-end test (fs-driven, no fixture edits)

| Test name | Scenario | Asserts |
|---|---|---|
| `test_integration_subtasks_full_pipeline` | Mirror `test_integration_multi_file_workspace` but with a 3-level tree; run `process_files_via_tree` + `generate_workspace`; read generated `lib.rs` and `Cargo.toml` from disk | Generated root `lib.rs` contains the expected `mod subtasks { ... }` block; generated root `Cargo.toml` lists all descendant crates as path deps; generated intermediate `lib.rs` has a smaller subtasks block scoped to its subtree |

### 7.5 What the impl-fixture flip will validate

The 3 ignored tests in `tests/typed_invocation.rs:264`, `:276`, `:291` exercise: (a) cross-file typed call, (b) child-exported type reachable through subtasks, (c) structural-only descendant reachable. Those tests stay `#[ignore]` after this task lands; `impl-fixture` un-ignores them by wiring the root RUNME.rs to actually call `subtasks::services::api_v2::deploy(...)`, `subtasks::structural_only::leaf::leaf_task(...)`, etc.

I'll verify those tests un-ignore cleanly by spot-checking the generated lib.rs contains the right paths, but I won't edit the tests.

---

## 8. Files touched

| File | Change |
|---|---|
| `src/bin/rnme/compile.rs` | Add `emit_subtasks_block(&ModuleNode) -> String` + `MergeNode` enum + `collect_descendant_crate_names` + `subtasks_dep_graph_is_acyclic`. Modify `node_to_crate_entry` to append the block to `lib_source` and the descendant deps to `cargo_toml`. Add `debug_assert!` at end of `flatten_tree_into_entries`. |
| `src/bin/rnme/codegen.rs` | Add `descendant_crate_names: Vec<String>` field to `CrateEntry` (per §6 open question 2 → my default is yes). |

No new files. No changes to `transform.rs`, `crate_name.rs`, the macros crate, or the engine. No changes to `ModuleNode`.

---

## 9. What this task does NOT do

- **No collision detection.** Two siblings normalizing to the same `module_name` would produce a duplicate `pub mod X` block today — that's task #17 (`collision-detection`). For this task I just emit whatever `module_name` says.
- **No fixture edits.** `impl-fixture` un-ignores the 3 tests.
- **No `[ignore]` flips.** Same as above.
- **No `transform_source` changes.** Per plan brief; emission stays in `compile.rs`.
- **No new `ModuleNode` fields.** Per plan brief.
- **No structural-dir normalization change** unless lead resolves §3.4 open question #1 differently — my default is to run intermediate components through the same `crate_name_from_path` segment normalizer, which is a no-op on clean idents like `services` and `structural_only`.

---

## 10. Open questions for the reviewer

1. **Structural-intermediate dir name normalization (§3.4).** Run through `crate_name_from_path` segment-by-segment (default), error if not a valid ident, or pass verbatim? Default: option 1.
2. **`descendant_crate_names` field on `CrateEntry` (§6).** Add it to make the cycle check and tests trivial, or keep `CrateEntry` minimal and reparse Cargo.toml? Default: add the field.
3. **`#[allow(unused_imports, dead_code)]` on the `mod subtasks` block (§3.1).** I'm adding both because `pub use ::child::*` will pull in items the parent doesn't reference, and unused `pub mod` chains will warn. Acceptable, or should I tighten this?
4. **Ordering inside the emitted block (§3.2).** Using `BTreeMap` for deterministic alphabetical ordering. Alternative: preserve `ModuleNode.children` order (depth-first discovery order). Alphabetical is more diff-friendly for test snapshots; discovery order matches what the rest of the code does. Default: BTreeMap.
5. **Single comment marker delimiting auto-generated cargo deps (§5).** Decorative; only affects human readability. Acceptable, or skip the marker?

Awaiting `lead` sign-off.
