# Multi-File Build System Implementation

## Goal

Implement the multi-file compilation model specified in `docs/build_system_design.md`. All discovered RUNME.rs files in a directory tree compile into a single binary via a generated Cargo workspace. Replace `#[runme::main]` with `#[runme::init]`, add task grouping via `GroupDef`, and evolve the compilation pipeline from single-file to multi-file workspace generation.

## Approach

Pure implementation following the detailed spec in `build_system_design.md`. The work splits into four independent-ish streams across three crates (runme, runme-macros, runme-cli), with a dependency chain between them. A research spike answers the open question about `inventory` linking behavior before workspace generation begins.

**Strategy per phase:**
- Phase 0: Subagent (single researcher)
- Phase 1: Agent Team (2 parallel implementors, no file overlap — different crates)
- Phase 2: Subagent (single implementor, depends on Phase 1)
- Phase 3: Subagent (single implementor, depends on Phase 2)
- Phase 4: Subagent Pool (parallel test writers + validator)

## Acceptance Criteria

- [ ] `#[runme::main]` macro is removed from `runme-macros`; all references removed from `runme` lib re-exports, prelude, and example
- [ ] `TaskDef` has a `group: &'static str` field; `#[task]` macro populates it from `__RUNME_GROUP` constant (falls back to `""`)
- [ ] `GroupDef` type exists with `key` and `display_name`; registered via `inventory`
- [ ] `InitDef` and `InitContext` types exist; `#[runme::init]` macro registers init hooks via `inventory`
- [ ] `InitContext.set_group_name()` overrides `GroupDef.display_name`
- [ ] Crate naming: relative path → valid Rust crate name (unit tested)
- [ ] Source transformation: strip shebang + inject `__RUNME_GROUP` constant (unit tested)
- [ ] Path dependency rewriting: relative paths resolved against original RUNME.rs location (unit tested)
- [ ] `compile.rs` generates a full Cargo workspace from a `DiscoveryResult`
- [ ] Cache directory keyed by hash of root RUNME.rs absolute path
- [ ] Generated workspace builds successfully with `cargo build`
- [ ] Runner crate's `main()` collects all tasks from all crates via `inventory`
- [ ] Init hooks run leaf-to-root
- [ ] CLI `main.rs` passes `DiscoveryResult` to new pipeline; shebang mode discovers full tree
- [ ] Integration test: multi-file tree generates correct workspace structure
- [ ] Integration test: compiled binary lists tasks from all RUNME.rs files
- [ ] Integration test: `#[runme::init]` group name override works
- [ ] Integration test: path dependency rewriting produces working build
- [ ] All existing tests pass (or are updated to match new `TaskDef` shape)
- [ ] Example `RUNME.rs` updated to new anatomy (no main)

## Human Review Gates

1. **After Phase 0 (Research)** — `inventory` linking results may change workspace generation approach. Auto-Approve: true. Rationale: low risk, confirmatory research. If `use x as _;` doesn't work, the fallback (dummy symbol) is straightforward.
2. **After Phase 1 (Foundation)** — Verify types and APIs look right before macros and workspace gen build on them. Human Review: true. Rationale: these types are the foundation; wrong shape here cascades everywhere.
3. **After Phase 4 (Validation)** — Final sign-off. Human Review: true. Rationale: this is a significant architectural change.

---

## Status: complete

## Context

### Key Files

**Spec:**
- `docs/build_system_design.md` — full specification (source of truth)
- `docs/system_design.md` — high-level architecture (updated to reference build_system_design.md)

**Code to modify:**
- `crates/runme/src/task.rs` — `TaskDef`, `Registry`, `TaskContext`
- `crates/runme/src/lib.rs` — re-exports
- `crates/runme/src/prelude.rs` — public API
- `crates/runme-macros/src/lib.rs` — `#[task]`, `#[runme::main]` (remove), `#[runme::init]` (add)
- `crates/runme-cli/src/compile.rs` — workspace generation (major rewrite)
- `crates/runme-cli/src/main.rs` — CLI entry point
- `crates/runme-cli/src/frontmatter.rs` — path dependency rewriting extension
- `crates/runme/examples/RUNME.rs` — update to new anatomy

**New files:**
- `crates/runme/src/init.rs` — `InitDef`, `InitContext`, `GroupDef`
- `crates/runme-cli/src/crate_name.rs` — path → crate name conversion
- `crates/runme-cli/src/transform.rs` — source transformation (shebang strip + group injection)

### Existing Patterns

- `inventory::collect!` / `inventory::submit!` for static registration (used by TaskDef)
- `proc_macro_attribute` with `syn`/`quote` for macro implementation
- `tempfile::TempDir` for filesystem tests in compile.rs and discover.rs
- Content hash + marker file pattern in current compile.rs (will be replaced)

---

## Team

| Name | Role | Agent Type | Model | Strategy |
|------|------|-----------|-------|----------|
| `inventory-researcher` | Research inventory linking behavior | Explore | opus | subagent |
| `lib-types` | Implement library types (GroupDef, InitDef, InitContext, TaskDef update) | general-purpose | opus | team |
| `cli-utils` | Implement CLI utility modules (crate naming, source transform, path rewriting) | general-purpose | opus | team |
| `macro-updater` | Update proc macros (remove main, update task, add init) | general-purpose | opus | subagent |
| `workspace-gen` | Implement workspace generation + CLI integration | general-purpose | opus | subagent |
| `test-unit` | Write unit tests for new utility functions | general-purpose | sonnet | subagent |
| `test-integration` | Write integration and E2E tests | general-purpose | opus | subagent |
| `validator` | Run full test suite, verify acceptance criteria | general-purpose | sonnet | subagent |
| `cleanup` | Update examples, final consistency check | general-purpose | sonnet | subagent |

---

## Phase 0: Research

### Task: `research-inventory-linking`

- **Assigned To:** `inventory-researcher`
- **Depends On:** none
- **Parallel:** standalone
- **Human Review:** false
- **Auto-Approve:** true

**Description:**

Determine whether `use some_crate as _;` is sufficient to prevent the linker from dead-stripping `inventory` registrations in a multi-crate workspace. The concern: if the runner crate depends on a lib crate but never references any symbols from it, the linker may optimize away the crate's object files, silently losing `inventory::submit!` registrations.

Research approach:
1. Read `inventory` crate source to understand the linking mechanism (linker sections, `ctor`, etc.)
2. Check inventory's documentation and issues for multi-crate usage guidance
3. If unclear from source: create a minimal repro — a workspace with a lib crate that registers an inventory item and a bin crate that only does `use lib as _;`, then check if the item appears in the binary's inventory iteration

**Expected output:** Clear answer: "yes, `use x as _;` works" or "no, need a stronger reference (here's what works)". Include evidence.

---

## Phase 1: Foundation

Two parallel streams — different crates, no file overlap.

### Task: `implement-lib-types`

- **Assigned To:** `lib-types`
- **Depends On:** none
- **Parallel:** yes (with `implement-cli-utils`)
- **Human Review:** false (reviewed at phase gate)
- **Plan Approval:** yes

**Description:**

Implement the core types in the `runme` library crate that everything else builds on. Reference `docs/build_system_design.md` §§ `#[runme::init]`, Task Groups.

Changes to `crates/runme/`:

1. **New file `src/init.rs`** — Add `InitDef`, `InitContext`, and `GroupDef` types:
   ```rust
   // GroupDef — registered via inventory, one per RUNME.rs file
   pub struct GroupDef {
       pub key: &'static str,
       // display_name starts as key, overridable via InitContext
   }

   // InitDef — registered via inventory by #[runme::init]
   pub struct InitDef {
       pub group: &'static str,
       pub func: fn(&mut InitContext),
   }

   // InitContext — passed to init functions
   pub struct InitContext {
       group_name: String,
   }
   impl InitContext {
       pub fn new(default_group: &str) -> Self { ... }
       pub fn set_group_name(&mut self, name: &str) { ... }
       pub fn group_name(&self) -> &str { ... }
   }
   ```
   Add `inventory::collect!` for both `InitDef` and `GroupDef`.
   `InitDef` and `GroupDef` need `Send + Sync` (same pattern as `TaskDef`).

2. **Update `src/task.rs`** — Add `group: &'static str` field to `TaskDef`. Update all static `TaskDef` literals in tests (add `group: ""`).

3. **Update `src/lib.rs`** — Add `pub mod init;` declaration.

4. **Update `src/prelude.rs`** — Export `InitDef`, `InitContext`, `GroupDef` from the new module.

5. **Remove `#[runme::main]` re-exports** — In `src/lib.rs`, remove `pub use runme_macros::main;`. In `src/prelude.rs`, remove `pub use runme_macros::main as runme_main;`.

6. **Verify** — `cargo build -p runme` succeeds. Existing unit tests pass (after adding `group: ""` to test TaskDef literals). Run `cargo test -p runme`.

### Task: `implement-cli-utils`

- **Assigned To:** `cli-utils`
- **Depends On:** none
- **Parallel:** yes (with `implement-lib-types`)
- **Human Review:** false (reviewed at phase gate)
- **Plan Approval:** yes

**Description:**

Implement the pure utility modules in the `runme-cli` crate that the workspace generator will use. These are self-contained functions with no dependency on the library type changes. Reference `docs/build_system_design.md` §§ Source Transformation, Crate Naming, Path Dependency Rewriting.

New files in `crates/runme-cli/src/`:

1. **`crate_name.rs`** — Convert a relative path to a valid Rust crate name:
   - `./RUNME.rs` (or empty path) → `"root"`
   - `services/auth/RUNME.rs` → `"services_auth"`
   - `web-app/RUNME.rs` → `"web_app"`
   - Rules: strip `RUNME.rs` filename, replace `/`, `-`, `.` with `_`, trim trailing `_`
   - Prefix with `runme_` if result is a Rust keyword or starts with a digit
   - Collision detection: function takes a list of paths and returns a map of path → crate name, panicking on collision
   - Unit tests for all edge cases (root, nested, dashes, dots, keywords, digits)

2. **`transform.rs`** — Source transformation:
   - `transform_source(source: &str, group: &str) -> String` — strips shebang (reuse existing `strip_shebang`), prepends `const __RUNME_GROUP: &str = "<group>";`
   - Unit tests: source with shebang, without shebang, with existing imports, group string with special chars

3. **Extend `frontmatter.rs`** — Path dependency rewriting:
   - `rewrite_path_deps(deps: &[(String, String)], original_dir: &Path) -> Vec<(String, String)>` — for each dependency, if the value contains `path = "..."`, resolve the path relative to `original_dir` and rewrite to absolute
   - Must handle both `path = "../foo"` (simple string value) and `{ path = "../foo", features = [...] }` (inline table)
   - Unit tests: no path deps (passthrough), simple path dep, inline table with path, mixed path and non-path deps, already-absolute paths (no-op)

4. **Register modules** — Add `mod crate_name;`, `mod transform;` to `main.rs` (or a module file).

5. **Verify** — `cargo test -p runme-cli` passes for the new unit tests. Existing discover/frontmatter tests still pass.

### Task: `validate-phase-1`

- **Assigned To:** `validator`
- **Depends On:** `implement-lib-types`, `implement-cli-utils`
- **Parallel:** no
- **Human Review:** true
- **Review Rationale:** Foundation types are the substrate — wrong shapes cascade. Verify before building macros and workspace gen on top.

**Description:**

Run the full test suite across all crates and verify Phase 1 acceptance criteria:

```bash
cargo test --workspace
```

Check:
- [ ] `GroupDef`, `InitDef`, `InitContext` types exist and compile
- [ ] `TaskDef` has `group` field; all test literals updated
- [ ] `#[runme::main]` re-exports removed from `lib.rs` and `prelude.rs`
- [ ] Crate naming function handles all edge cases
- [ ] Source transformation strips shebang and injects group constant
- [ ] Path dependency rewriting resolves relative paths correctly
- [ ] All existing tests pass

---

## Phase 2: Macros

### Task: `implement-macros`

- **Assigned To:** `macro-updater`
- **Depends On:** `validate-phase-1`
- **Parallel:** no
- **Human Review:** false
- **Plan Approval:** yes

**Description:**

Update the proc-macro crate to match the new design. Reference `docs/build_system_design.md` §§ `#[runme::init]`, Task Groups, RUNME.rs File Anatomy. Read the existing macro code at `crates/runme-macros/src/lib.rs` carefully before making changes.

Changes to `crates/runme-macros/src/lib.rs`:

1. **Remove `#[runme::main]`** — Delete the entire `pub fn main(...)` proc macro function.

2. **Update `#[runme::task]`** — Add `group` field to the generated `TaskDef` literal. The macro should emit:
   ```rust
   group: {
       #[allow(dead_code)]
       const DEFAULT: &str = "";
       #[cfg(__runme_group)]
       { __RUNME_GROUP }
       #[cfg(not(__runme_group))]
       { DEFAULT }
   }
   ```
   Actually, simpler approach: just reference `__RUNME_GROUP` directly. If it doesn't exist (e.g., in tests), the code won't compile. For tests, either:
   - Define `const __RUNME_GROUP: &str = "";` in test modules
   - Or use a fallback approach: try to reference `__RUNME_GROUP`, fall back to `""` at compile time

   The cleanest approach: the macro always emits `group: __RUNME_GROUP`. The code generator always injects the constant. For direct usage in tests, define the constant manually. This is simplest and most explicit.

   Update the `inventory::submit!` block to include `group: __RUNME_GROUP`.

3. **Add `#[runme::init]`** — New proc macro attribute:
   - Accepts a function with signature `fn(ctx: &mut InitContext)` (or no args)
   - Generates a wrapper + `inventory::submit!` for `InitDef`
   - Similar pattern to `#[task]` but simpler (no async, no return type variants)
   - The generated `InitDef` includes `group: __RUNME_GROUP`

4. **Update example** — `crates/runme/examples/RUNME.rs`:
   - Remove `#[runme::main] fn main() {}`
   - Add `const __RUNME_GROUP: &str = "";` at top (since examples aren't going through the code generator)
   - Keep the task definitions

5. **Verify** — `cargo build --workspace` succeeds. `cargo test --workspace` passes. The example compiles (may need to adjust how examples are built — they need a `fn main()` to be runnable directly, but that's the code generator's job in the real pipeline; for the example, add a manual main that builds registry and dispatches).

### Task: `validate-phase-2`

- **Assigned To:** `validator`
- **Depends On:** `implement-macros`
- **Parallel:** no
- **Human Review:** false
- **Auto-Approve:** true
- **Review Rationale:** Macro changes are constrained by the spec and verified by compilation. Low risk.

**Description:**

```bash
cargo test --workspace
```

Check:
- [ ] `#[runme::main]` no longer exists as a macro
- [ ] `#[task]` emits `group: __RUNME_GROUP` in the TaskDef
- [ ] `#[runme::init]` compiles and registers an `InitDef`
- [ ] Example RUNME.rs compiles
- [ ] All tests pass

---

## Phase 3: Workspace Generation

### Task: `implement-workspace-gen`

- **Assigned To:** `workspace-gen`
- **Depends On:** `validate-phase-2`, `research-inventory-linking`
- **Parallel:** no
- **Human Review:** false
- **Plan Approval:** yes

**Description:**

The big piece: evolve `compile.rs` from single-file project generation to multi-file workspace generation. Reference `docs/build_system_design.md` §§ Pipeline, Cache Directory, Workspace Structure, Generated Runner Crate, Generated Crate Cargo.toml. Also incorporate the findings from `research-inventory-linking` for how to ensure crates are linked.

Changes to `crates/runme-cli/src/compile.rs`:

1. **New entry point** — `compile_workspace(discovery: &DiscoveryResult) -> Result<CompileResult, CompileError>`:
   - Compute cache directory from hash of root RUNME.rs absolute path
   - Generate workspace structure (always, not conditional on hash)
   - Run `cargo build`
   - Return path to runner binary

2. **Cache directory** — `fn cache_dir_for_root(root_runme: &Path) -> PathBuf`:
   - Hash the absolute path of the root RUNME.rs
   - `~/.cache/runme/<hash-prefix>/`

3. **Workspace generation** — `fn generate_workspace(cache_dir: &Path, discovery: &DiscoveryResult) -> Result<(), CompileError>`:
   - Collect all RUNME.rs files: `discovery.nearest` + `discovery.children`
   - For each file:
     - Compute relative path from discovery root
     - Derive crate name (using `crate_name.rs`)
     - Read source, transform (using `transform.rs`)
     - Parse frontmatter, rewrite path deps (using `frontmatter.rs`)
     - Write `<crate_name>/Cargo.toml` and `<crate_name>/src/lib.rs`
   - Generate workspace `Cargo.toml` with all members
   - Generate runner crate:
     - `Cargo.toml` depends on all lib crates + `runme`
     - `main.rs` with `use <crate> as _;` for each lib crate, plus the main function body

4. **Runner main.rs generation** — The generated `fn main()`:
   ```rust
   fn main() {
       runme::tokio::runtime::Builder::new_multi_thread()
           .enable_all()
           .build()
           .expect("failed to create tokio runtime")
           .block_on(async {
               // Run init hooks (leaf-to-root ordering)
               // Build registry from inventory
               // Parse CLI args, dispatch
               let registry = runme::task::Registry::from_inventory();
               let args: Vec<String> = std::env::args().collect();
               // ... (same dispatch logic as old #[runme::main], but with init)
           });
   }
   ```
   Init ordering: the generated main needs to know the depth of each file to sort leaf-to-root. The code generator can emit this as metadata alongside each `InitDef`, or sort by group key depth (count of `/` separators).

5. **Update CLI `main.rs`** — Replace the current single-file flow:
   - Discovery mode: pass full `DiscoveryResult` to `compile_workspace`
   - Shebang mode: discover from the file's directory, then `compile_workspace`
   - Exec the runner binary (same as today)

6. **Keep old `compile()` function** — Mark deprecated or remove. The new `compile_workspace()` handles single-file as a degenerate case.

7. **Verify** — `cargo build --workspace` succeeds. Manual smoke test: create a temp directory with 2-3 RUNME.rs files, run `cargo run -p runme-cli`, verify it generates the workspace, builds, and the binary lists tasks from all files.

---

## Phase 4: Testing & Cleanup

### Task: `write-unit-tests`

- **Assigned To:** `test-unit`
- **Depends On:** `implement-workspace-gen`
- **Parallel:** yes (with `write-integration-tests`)
- **Human Review:** false

**Description:**

Ensure comprehensive unit test coverage for the new utility modules. Check what tests were already written in Phase 1 and fill gaps.

Files to verify/extend:
- `crates/runme-cli/src/crate_name.rs` — edge cases: empty path, deeply nested, unicode in path names, names that collide
- `crates/runme-cli/src/transform.rs` — multi-line shebangs (shouldn't exist but be defensive), source with `//!` frontmatter + shebang, empty source
- `crates/runme-cli/src/frontmatter.rs` — path rewriting: Windows-style paths (if relevant), paths with spaces, symlinks
- `crates/runme/src/init.rs` — `InitContext` new/get/set, `GroupDef` inventory collection

### Task: `write-integration-tests`

- **Assigned To:** `test-integration`
- **Depends On:** `implement-workspace-gen`
- **Parallel:** yes (with `write-unit-tests`)
- **Human Review:** false

**Description:**

Write integration tests that exercise the full pipeline. These go in `crates/runme-cli/tests/` or as `#[cfg(test)]` in compile.rs.

**Test 1: Single-file workspace generation**
- Create a temp dir with one RUNME.rs (no main, has `#[task]`)
- Call `compile_workspace` with a `DiscoveryResult` containing just this file
- Verify workspace structure: workspace Cargo.toml, one lib crate (named "root"), runner crate
- Verify lib crate's `src/lib.rs` has the `__RUNME_GROUP` constant injected
- Verify runner crate's `Cargo.toml` depends on "root" crate
- Verify `cargo build` in the workspace succeeds
- Run the binary with `--list`, verify the task appears

**Test 2: Multi-file workspace generation**
- Create a temp dir tree:
  ```
  tmp/
    RUNME.rs          (defines task "root_task")
    services/
      auth/
        RUNME.rs      (defines task "auth_task")
    web/
      RUNME.rs        (defines task "web_task")
  ```
- Call `compile_workspace`
- Verify 3 lib crates + runner crate generated
- Verify crate names: `root`, `services_auth`, `web`
- Verify each lib.rs has correct `__RUNME_GROUP` (".", "services/auth", "web")
- Build and run with `--list`, verify all 3 tasks appear with correct groups

**Test 3: Init hook with group name override**
- Create a RUNME.rs with `#[runme::init]` that calls `ctx.set_group_name("My Custom Name")`
- Build, run, verify group display name is overridden

**Test 4: Path dependency rewriting**
- Create a temp dir with a RUNME.rs that has a frontmatter path dependency pointing to a sibling directory
- Create the sibling directory with a minimal Cargo crate
- Verify the generated Cargo.toml has the absolute path
- Verify the workspace builds successfully

**Test 5: Init ordering (leaf-to-root)**
- Create a tree with root and child RUNME.rs files, each with `#[runme::init]` that prints to stderr or appends to a shared mechanism
- Verify child init runs before root init

**Test 6: Shebang mode discovers full tree**
- Create a multi-file tree
- Invoke the CLI with a specific RUNME.rs file path (shebang mode)
- Verify tasks from ALL files are available, not just the invoked file

### Task: `cleanup-and-validate`

- **Assigned To:** `cleanup`
- **Depends On:** `write-unit-tests`, `write-integration-tests`
- **Parallel:** no
- **Human Review:** false

**Description:**

Final cleanup pass:

1. Update `crates/runme/examples/RUNME.rs` to the new anatomy:
   - No `fn main()`
   - No `#[runme::main]`
   - Define `const __RUNME_GROUP: &str = "";` for standalone compilation
   - Keep as a reference showing the expected RUNME.rs format
   - If the example can't compile standalone without main, convert it to a doc example or move it to tests

2. Check for any remaining references to `#[runme::main]` or `runme_main` in:
   - Source code (grep across workspace)
   - Documentation files
   - Comments

3. Verify `build_system_design.md` "Current State" section is still accurate or add a note that it describes the pre-implementation state.

4. Run full test suite:
   ```bash
   cargo test --workspace
   cargo clippy --workspace
   ```

### Task: `final-validation`

- **Assigned To:** `validator`
- **Depends On:** `cleanup-and-validate`
- **Parallel:** no
- **Human Review:** true
- **Review Rationale:** Major architectural change complete. Final human sign-off before merging.

**Description:**

Run the full validation suite and check every acceptance criterion:

```bash
cargo test --workspace
cargo clippy --workspace
```

Walk through each acceptance criterion from the top of this plan and mark it pass/fail. Report results.

---

## Validation Profile

```yaml
validation:
  build:
    command: "cargo build --workspace"
    required: true
  tests:
    command: "cargo test --workspace"
    required: true
  clippy:
    command: "cargo clippy --workspace"
    required: false
  grep-old-main:
    command: "grep -r 'runme::main' crates/ --include='*.rs' || true"
    required: true
    expected: "no matches (empty output)"
```

---

## Findings

### `research-inventory-linking`

**Result:** `use x as _;` is **NOT sufficient**. The linker doesn't see `use` statements — it only includes crate object files when actual symbols are referenced. Without a real symbol reference, `inventory` registrations are silently dropped. This is confirmed by `inventory` crate discussions, `ctor` crate issues, and Rust linkage docs.

**Solution:** Each generated lib crate exports `pub fn __runme_link() {}`. The runner's `main()` calls `crate_name::__runme_link()` for each crate. The code generator already knows all crate names, so this is trivial to emit. `build_system_design.md` updated with this approach.

---

## Decisions Log

*(populated during execution)*

---

## Blockers

*(populated during execution)*
