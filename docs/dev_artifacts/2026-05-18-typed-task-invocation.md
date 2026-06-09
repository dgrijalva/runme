# Typed Task Invocation — Implementation Plan

**Date:** 2026-05-18
**Status:** complete
**Design doc:** `docs/invoking_tasks.md`
**Scope:** Replace string-keyed `ctx.run` as the default in-process task-invocation API. Add a typed-builder shim emitted by `#[rnme::task]`, a `subtasks` module tree auto-injected at each parent RUNME crate for cross-file calls, and `[rnme.rename]` frontmatter as the Class-2 collision escape hatch.

## Goal

End state: an author writing `build_wasm(ctx, true, false).await?` inside a RUNME.rs gets a real child task (registered, log-routed, ready-signaled, cancelable, optionally timed out) — not the inlined body that today's direct call produces. A parent RUNME.rs gets typed access to descendant tasks via `subtasks::services::api::deploy(ctx, args).await?`, with `.spawn()?` available for detached execution. The string-keyed `ctx.run` path keeps working for discovery, MCP, and CLI re-entry.

## Approach

Mechanical refactor of three coupled subsystems, in this order:

1. **Engine.** Add a future-factory invocation path alongside the existing string-args path. Both produce the same `TaskHandle` via `EngineInternals::spawn_child`.
2. **Macro.** Rewrite `#[rnme::task]` so the user's body moves to a private symbol, and the public symbol becomes a thin shim returning a `TaskBuilder` configured with a typed-args closure.
3. **Codegen.** Have the workspace generator emit `mod subtasks { ... }` into each parent crate, materializing the directory-mirrored tree of descendant RUNME.rs files. Wire `[rnme.rename]` through frontmatter parsing and the crate-name + group-key + module-path derivation so the rename applies uniformly to all three.

The design doc `docs/invoking_tasks.md` is the authoritative reference for behavior. This plan describes *how* to land it; it does not re-decide design questions.

## Acceptance Criteria

- [ ] `build_wasm(ctx, true, false).await?` inside a RUNME.rs invokes through the framework (separate child task, distinct log source, ready-state propagation works).
- [ ] `build_wasm(ctx, true, false).spawn()?` returns a `TaskHandle`; `wait_ready()` and other handle ops work as expected.
- [ ] Calling a task fn without `.await?` or `.spawn()?` produces an `unused_must_use` warning. `TaskBuilder` and `SpawnBuilder` both carry `#[must_use]`.
- [ ] Cross-file: `subtasks::child_dir::task_name(ctx, args).await?` from a parent RUNME.rs invokes the child's task as a framework-integrated child task.
- [ ] Cross-file types travel with their tasks: a Form-3 task's args struct is reachable as `subtasks::child_dir::ArgsStructName`.
- [ ] Adding a RUNME.rs at an intermediate directory does not break existing `subtasks::descendant::...` paths in ancestor RUNMEs.
- [ ] `ctx.run("group/path:task_name", &[...])` continues to resolve and execute the same task as the typed path. Both paths converge on the same registry.
- [ ] `[rnme.rename]` in a child RUNME.rs substitutes its name *before* normalization; the substituted name appears uniformly as the cargo crate name, the `subtasks` module path, and the inventory group key (so `ctx.run("renamed:task", ...)` works under the new name).
- [ ] Sibling normalization collision (e.g. `foo-bar/` + `foo_bar/`) without a rename produces a build error that names both paths and includes a paste-ready `[rnme.rename]` snippet.
- [ ] `cargo build --workspace` and `cargo test --workspace` pass on the rnme repo.
- [ ] An integration-test fixture RUNME tree (added by this plan) exercises typed in-file calls, typed cross-file calls, `[rnme.rename]`, and collision-detection error reporting.

## Human Review Gates

| # | When | What's reviewed | Auto-Approve | Rationale |
|---|------|-----------------|--------------|-----------|
| G1 | After Phase 1 | Engine dispatch shape; `Invocation` enum signature; `TaskDef` named-static pattern. Confirm before macro work commits to these. | false | Architectural — the macro's emitted code is shaped by these decisions; changes after are expensive. |
| G2 | After Phase 2 | Typed-shim-macro output against a hand-written RUNME.rs fixture. Verify in-file calls work end-to-end before cross-file work begins. | false | Validates the central macro rewrite, which is the largest single change and the riskiest. |
| G3 | After Phase 3 | Cross-file `subtasks` paths work against a multi-RUNME fixture; intermediate-RUNME addition is non-breaking. | false | Validates the cross-file claim that motivates the design. |
| G4 | Final | Whole-plan acceptance criteria pass; integration fixture runs green; manual spot-check against the search_agent RUNME.rs from the working monorepo. | false | Final sign-off before merge. |

## Status

complete — landed 2026-05-19

Known gaps accepted at G4 sign-off:
- No live integration test for `.spawn()?` + `.wait_ready()` chain on a typed shim. Statically verified by the macro emission; not runtime-exercised in the fixture.
- Manual spot-check against `search_agent/RUNME.rs` in the working monorepo is the user's responsibility (out of agent reach).

## Context

The design was developed across an extensive conversation captured in `docs/invoking_tasks.md`. Surface-area research was completed in conversation and identified these as the touched files:

**rnme library:**
- `src/task.rs` — `TaskDef`, `TaskFn`, `TaskFnKind`, `TaskContext::run`
- `src/execution/builder.rs` — `TaskBuilder` shape and `IntoFuture` impl
- `src/execution/engine.rs` — `EngineInternals::spawn_child`
- `src/execution/execution.rs` — `spawn_body` (the central join point at line 451)

**macro crate:**
- `macros/src/lib.rs` — `#[rnme::task]` proc-macro

**CLI / build system:**
- `src/bin/rnme/transform.rs` — source transformation (`__RNME_GROUP`, `__rnme_link`)
- `src/bin/rnme/compile.rs` — workspace generation, per-crate Cargo.toml emission
- `src/bin/rnme/codegen.rs` — runner crate generation
- `src/bin/rnme/frontmatter.rs` — `//! [dependencies]` parsing
- `src/bin/rnme/crate_name.rs` — path → Rust ident normalization
- `src/bin/rnme/discover.rs` — RUNME.rs discovery

**Existing patterns to preserve:**
- Group names continue to be derived from the directory path; `[rnme.rename]` substitutes the dir-name before normalization, applied uniformly.
- `__rnme_link()` linker-pull-in unchanged.
- `inventory::submit!` continues to be how tasks register.
- Dynamic-path tasks (registered via `register_task`) remain untyped; only static `#[rnme::task]` tasks get typed shims.

## Execution Strategy

**Agent Team** for implementors. The macro / engine / codegen changes are tightly coupled at type-signature boundaries; implementors will need to coordinate on exact shapes (the future-factory type, the typed-args closure signature, the named-static TaskDef path). A persistent team with cross-messaging beats a subagent pool here.

Validators are **named subagents** (one per implementor), Sonnet, paired with their implementor's task. Validators run cargo + the integration fixture and check off acceptance-criteria items.

## Team

| Name | Role | Type | Model | Plan Approval Required |
|---|---|---|---|---|
| `lead` | Planning lead; reviews implementor plans before code is written; owns gate decisions | general-purpose | opus | n/a |
| `impl-engine-dispatch` | Implementor — Engine-side future-factory dispatch | general-purpose | opus | yes |
| `impl-taskdef-static` | Implementor — Macro emits `TaskDef` as a named static | general-purpose | opus | yes |
| `impl-frontmatter-rename` | Implementor — Parse `[rnme.rename]` in frontmatter | general-purpose | opus | yes |
| `impl-typed-shim-macro` | Implementor — Macro rewrite: rename body to private symbol, emit typed-builder shim at original name | general-purpose | opus | yes |
| `impl-apply-rename` | Implementor — Apply `[rnme.rename]` uniformly to crate name, group key, and module path | general-purpose | opus | yes |
| `impl-subtasks-injection` | Implementor — Generate `mod subtasks { ... }` per parent crate; emit transitive cargo deps | general-purpose | opus | yes |
| `impl-collision-detection` | Implementor — Detect sibling normalization collisions; emit paste-ready error | general-purpose | sonnet | no |
| `impl-test-audit` | Implementor — Audit and update tests that build `TaskContext` directly and call task fns | general-purpose | opus | yes |
| `impl-fixture` | Implementor — Build the integration-test fixture monorepo and acceptance harness | general-purpose | opus | yes |
| `val-engine` | Validator for `impl-engine-dispatch` | Bash | sonnet | n/a |
| `val-taskdef` | Validator for `impl-taskdef-static` | Bash | sonnet | n/a |
| `val-frontmatter` | Validator for `impl-frontmatter-rename` | Bash | sonnet | n/a |
| `val-typed-shim` | Validator for `impl-typed-shim-macro` (uses fixture) | general-purpose | sonnet | n/a |
| `val-rename` | Validator for `impl-apply-rename` | general-purpose | sonnet | n/a |
| `val-subtasks` | Validator for `impl-subtasks-injection` (uses fixture) | general-purpose | sonnet | n/a |
| `val-collision` | Validator for `impl-collision-detection` | general-purpose | sonnet | n/a |
| `val-tests` | Validator for `impl-test-audit` (runs cargo test --workspace) | Bash | sonnet | n/a |
| `val-final` | End-to-end validator: runs full acceptance suite | general-purpose | sonnet | n/a |

Team composition: 10 implementors (1 sonnet, 9 opus), 9 validators (sonnet). Total: 19 agents + 1 lead.

## Phases

### Phase 1 — Foundation (parallel)

Three independent groundwork tasks. All can run in parallel; nothing in this phase touches the macro's typed-shim emission yet.

- **`engine-dispatch`** — `impl-engine-dispatch` → validated by `val-engine`
- **`taskdef-static`** — `impl-taskdef-static` → validated by `val-taskdef`
- **`frontmatter-rename`** — `impl-frontmatter-rename` → validated by `val-frontmatter`

→ **Gate G1** (after all three validate) — review the `Invocation` enum shape, the named-static `TaskDef` reference pattern, and the `Frontmatter.rename` field.

### Phase 2 — Macro + uniform rename (parallel where possible)

- **`typed-shim-macro`** — `impl-typed-shim-macro` → validated by `val-typed-shim` (depends on G1)
- **`apply-rename`** — `impl-apply-rename` → validated by `val-rename` (depends on `frontmatter-rename`)

These two can run in parallel since they touch different files. `typed-shim-macro` is the heaviest task in the plan; expect it to dominate phase wall-time.

→ **Gate G2** (after `val-typed-shim` passes) — confirm in-file typed calls work against a fixture before pushing on cross-file.

### Phase 3 — Cross-file plumbing

- **`subtasks-injection`** — `impl-subtasks-injection` → validated by `val-subtasks` (depends on G2 + `val-rename`)

This is where `mod subtasks` actually gets emitted and parent crates start cargo-depending on descendants. The fixture needs to exist by now for validation.

→ **Gate G3** — cross-file paths resolve; intermediate-RUNME addition is non-breaking.

### Phase 4 — Polish

- **`collision-detection`** — `impl-collision-detection` → validated by `val-collision` (depends on `val-subtasks`)
- **`test-audit`** — `impl-test-audit` → validated by `val-tests` (can start during Phase 3 in parallel; needed for final gate)
- **`fixture`** — `impl-fixture` builds the acceptance fixture. **Starts at the beginning of Phase 2** so it's ready in time for `val-typed-shim` and `val-subtasks`.

→ **Gate G4** — `val-final` runs the full acceptance checklist; lead confirms ready to merge.

## Tasks

### `engine-dispatch`

**Phase:** 1
**Depends on:** none
**Assigned to:** `impl-engine-dispatch`
**Validator:** `val-engine`
**Plan approval required:** yes (G1)

**Description:**

Introduce a future-factory invocation path through the engine, alongside the existing `Vec<String>` args path. Both paths must produce identical `TaskHandle`s and converge on the same `EngineInternals::spawn_child`.

Concrete work:
1. Define a new type, scoped to `src/execution/`, that unifies the two invocation modes. Suggested shape:
   ```rust
   type FutureFactory = Box<dyn for<'a> FnOnce(&'a TaskContext) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>> + Send>;
   pub enum Invocation {
       Strings(Vec<String>),
       Factory(FutureFactory),
   }
   ```
   Final shape to be confirmed during plan approval. The HRTB/lifetime modeling here is the tricky part; expect to iterate.
2. Change `EngineInternals::spawn_child` (`src/execution/engine.rs` line 390) to take `Invocation` instead of `Vec<String>`.
3. Change `TaskExecution::spawn_body` (`src/execution/execution.rs` line 358) to dispatch: when `Invocation::Strings(args)`, do exactly what it does today (`task.func.call(&body_ctx, &args)`); when `Invocation::Factory(f)`, call `f(&body_ctx)` and instrument-and-await the resulting future the same way.
4. Update all existing callers to wrap their args in `Invocation::Strings(args)`. Existing callers:
   - `TaskBuilder::spawn` (`src/execution/builder.rs` line 100) — wraps `inner.args` for now; the factory variant gets wired in Phase 2's `typed-shim-macro` work.
   - Any test callers of `spawn_child` (line 723 `spawn_task` and engine.rs tests).

No behavior change for existing callers; tests should pass unchanged.

**Acceptance:**
- [ ] `cargo build` passes.
- [ ] `cargo test --workspace` passes with no test changes.
- [ ] Code review: `spawn_body`'s line 451 dispatch is symmetric across both variants (same TaskContext setup, same instrumentation, same error handling).

**Approval gate before coding:** The implementor proposes the exact `Invocation` / `FutureFactory` type signatures to `lead` and waits for sign-off. Reason: these signatures are the contract that `typed-shim-macro` codegen will rely on; iterating on them later means re-running the macro work.

---

### `taskdef-static`

**Phase:** 1
**Depends on:** none
**Assigned to:** `impl-taskdef-static`
**Validator:** `val-taskdef`
**Plan approval required:** yes (G1)

**Description:**

Change `#[rnme::task]` to emit each `TaskDef` as a named static, then submit it to inventory by reference. This lets the typed shim (added in Phase 2) refer to the same static and avoid re-creating it.

Concrete work:
1. In `macros/src/lib.rs`, modify the expansion at line 481+ so instead of `inventory::submit! { TaskDef { ... } }`, it emits:
   ```rust
   pub static __RNME_TASKDEF_<NAME>: rnme::task::TaskDef = TaskDef { ... };
   ::rnme::inventory::submit! { &__RNME_TASKDEF_<NAME> }
   ```
   (Final naming convention and pub-ness to be confirmed during plan approval.)
2. Verify `inventory::submit!` accepts a reference expression to a static (it should — inventory works on values, and the existing path inlines a value literal). Prototype with a single macro invocation in a sandbox if unsure.
3. Ensure the static is `pub` enough to be referenced from the typed shim (which is in the same module) but doesn't pollute the crate's public surface — `pub(crate)` or just `pub` are both options.

No behavior change for existing inventory consumers; `Registry::from_inventory` collects `&'static TaskDef` either way.

**Acceptance:**
- [ ] `cargo build` passes.
- [ ] `cargo test --workspace` passes.
- [ ] Spot-check: a compiled RUNME.rs file emits a `__RNME_TASKDEF_<name>` symbol that the typed shim can reference.

**Approval gate before coding:** Implementor confirms the naming convention and visibility level with `lead`.

---

### `frontmatter-rename`

**Phase:** 1
**Depends on:** none
**Assigned to:** `impl-frontmatter-rename`
**Validator:** `val-frontmatter`
**Plan approval required:** yes (G1)

**Description:**

Extend `src/bin/rnme/frontmatter.rs` to parse `[rnme.rename]` sections.

Concrete work:
1. Add an optional field to `Frontmatter`:
   ```rust
   pub struct Frontmatter {
       pub dependencies: Vec<(String, String)>,
       pub rename: Option<String>,  // the `name = "..."` value, if present
   }
   ```
2. Extend `parse_frontmatter` to recognize the `[rnme.rename]` section and read its `name = "value"` line.
3. The rename value is the *substituted* name (per design doc: "sub in the new name before normalization"). Store it raw; normalization happens later in `apply-rename`.
4. Add tests covering: present and absent rename, malformed rename (warn or error per design call), rename alongside `[dependencies]`.

No behavior wiring yet — this just exposes the field. `apply-rename` consumes it.

**Acceptance:**
- [ ] Unit tests cover present, absent, and malformed cases.
- [ ] `cargo test --workspace` passes.
- [ ] `Frontmatter` field is reachable from `compile.rs`.

---

### `typed-shim-macro`

**Phase:** 2
**Depends on:** `engine-dispatch`, `taskdef-static` (Gate G1)
**Assigned to:** `impl-typed-shim-macro`
**Validator:** `val-typed-shim`
**Plan approval required:** yes (separately, before code is written)

**Description:**

The largest single change. Rewrite `#[rnme::task]` so the user's body is renamed to a private symbol, and the public symbol becomes a thin shim returning a `TaskBuilder`.

Concrete work:
1. In `macros/src/lib.rs`, change the expansion so:
   - User's `async fn build_wasm(ctx, release: bool, watch: bool) -> TaskResult { body }` is renamed to `__rnme_body_build_wasm(ctx, release, watch) -> TaskResult { body }` (private to the crate, or `pub(crate)` — implementor proposes).
   - A new public fn `build_wasm(ctx: &TaskContext, release: bool, watch: bool) -> TaskBuilder` is emitted at the original name. Its body:
     - Captures the typed args by value.
     - Constructs a `FutureFactory` closure that, given a `&TaskContext`, calls `__rnme_body_build_wasm(child_ctx, release, watch)` and boxes the future.
     - Builds a `TaskBuilder` carrying `Invocation::Factory(factory)` plus the `&__RNME_TASKDEF_build_wasm` reference.
2. Handle all three argument forms (zero-args, simple-args, parser-struct). Zero-args: no captures. Simple-args: capture each primitive by value. Parser-struct: capture the single struct by value.
3. The `start_task` injection (today at line 357-360) continues to happen inside the renamed body, not the shim.
4. Tag `TaskBuilder` with `#[must_use]` in `src/execution/builder.rs`. Also tag `SpawnBuilder` for consistency.
5. The existing string-args wrapper (`__runme_taskfn_<name>`, line 437+) stays — it's what `TaskDef.func` points to, used by the dynamic path. Don't remove it.

Edge case: `mode = cli|tui` attribute on `#[rnme::task]` must continue to flow into the emitted `TaskDef.ui_hint`. The shim itself doesn't carry this — only the TaskDef does.

Edge case: doc comment as description must continue to work. The shim doesn't need a description; the TaskDef does.

**Acceptance:**
- [ ] `cargo build --workspace` passes.
- [ ] `cargo test --workspace` passes (some pre-existing tests may need updating if they directly invoke the body symbol; those updates belong to `test-audit`, not this task).
- [ ] In the fixture: `build_wasm(ctx, true, false).await?` runs a separate child task observable via `TaskQuery` or by inspecting the registry.
- [ ] Calling `build_wasm(ctx, true, false);` (no await) emits an `unused_must_use` warning.
- [ ] `ctx.run("build_wasm", &["--watch"])` still resolves and runs the same body, going through the string path.

**Approval gate before coding:** Implementor writes a 1-page proposal describing (a) the exact signature of the emitted shim for each of the three arg forms, (b) the closure capture strategy for typed args, (c) how the `mode` and doc-description metadata flow through. `lead` reviews and signs off before macro code is written. Reason: this is the biggest single change and the riskiest; cheap to iterate on a written proposal, expensive to iterate on code.

---

### `apply-rename`

**Phase:** 2
**Depends on:** `frontmatter-rename`
**Assigned to:** `impl-apply-rename`
**Validator:** `val-rename`
**Plan approval required:** yes

**Description:**

Make `[rnme.rename]` propagate uniformly to the three places that derive identifiers from a RUNME.rs's directory path: the cargo crate name, the inventory group key (`__RNME_GROUP`), and the `subtasks` module path.

Concrete work:
1. In `src/bin/rnme/compile.rs`, when building a `CrateEntry`, look up the entry's parsed frontmatter and, if `rename` is present, substitute it for the directory's basename *before* calling the path-to-ident normalizer in `crate_name.rs`.
2. The same substituted-then-normalized name becomes:
   - The cargo crate name (today: `crate_name` field on `CrateEntry`)
   - The `group_key` field on `CrateEntry`
   - The constant value injected for `__RNME_GROUP` (via `transform_source`)
3. Source the rename from a single helper so all three call sites cannot drift apart. Suggested: add a `resolved_name(&self) -> String` method on `CrateEntry` or its source struct.
4. The `subtasks` module path emission (added by `subtasks-injection`) reads the same resolved name.

**Acceptance:**
- [ ] `cargo build --workspace` passes.
- [ ] Test: a RUNME.rs with `[rnme.rename] name = "foo_bar_v2"` produces a cargo crate named `foo_bar_v2`, a group key `foo_bar_v2`, and `__RNME_GROUP = "foo_bar_v2"`. Verified by reading the generated workspace.
- [ ] Test: `ctx.run("foo_bar_v2:task_name", &[])` resolves correctly (i.e., the dynamic path agrees with the renamed group).

---

### `subtasks-injection`

**Phase:** 3
**Depends on:** `typed-shim-macro` (Gate G2), `apply-rename`
**Assigned to:** `impl-subtasks-injection`
**Validator:** `val-subtasks`
**Plan approval required:** yes

**Description:**

Emit `mod subtasks { ... }` into each generated lib crate's source, materializing the directory-mirrored tree of descendant RUNME.rs files. Add cargo deps from each parent crate to every descendant crate in its discovered subtree.

Concrete work:
1. In `src/bin/rnme/compile.rs`, after computing all `CrateEntry`s and resolving their identifiers, build, per entry, the set of descendant entries (those whose paths are inside this entry's directory).
2. For each entry with at least one descendant, generate a `mod subtasks { ... }` source block by recursively walking the descendant paths. Per the design doc:
   ```rust
   mod subtasks {                                       // not pub
       pub mod services {
           pub use ::services_crate::*;                 // task shims + types + helpers
           pub mod api {
               pub use ::api_crate::*;
           }
       }
       pub mod service_common {                         // structural — no RUNME.rs here
           pub mod api_client {
               pub use ::api_client_crate::*;
           }
       }
   }
   ```
   - Each "intermediate" directory (no RUNME.rs but on the path to a descendant) emits an empty structural `pub mod` with no `pub use`.
   - Each "real" directory (has a RUNME.rs) emits a `pub mod` with a `pub use ::<crate>::*` at its root.
3. Append the generated block to the entry's `lib_source`. Keep `transform_source` file-local; do the appending in `compile.rs`.
4. Extend each entry's Cargo.toml to include `<descendant_crate> = { path = "../<descendant_crate>" }` for every descendant crate.
5. Add a debug_assert that the dep graph is acyclic (it must be — parent depends on descendants only).

**Acceptance:**
- [ ] `cargo build --workspace` passes on the rnme repo.
- [ ] Fixture: from a parent RUNME.rs, `subtasks::child::task(ctx, args).await?` runs the child's task as a framework-integrated child.
- [ ] Fixture: types exported by a child are reachable via `subtasks::child::TypeName`.
- [ ] Fixture: adding a RUNME.rs at an intermediate directory does not break ancestor RUNMEs' `subtasks::descendant::...` paths.
- [ ] Generated Cargo.toml in the workspace cache shows the expected transitive path-deps.

---

### `collision-detection`

**Phase:** 4
**Depends on:** `subtasks-injection`
**Assigned to:** `impl-collision-detection`
**Validator:** `val-collision`
**Plan approval required:** no (mechanical)

**Description:**

When two sibling RUNME.rs files end up at the same normalized name inside a parent's `subtasks::...` module and neither has a `[rnme.rename]`, emit a build error before generating the parent's source.

Concrete work:
1. In `compile.rs`, during `subtasks` tree assembly, detect duplicate normalized names at any module level.
2. If detected, abort codegen with an error message that:
   - Names both colliding directory paths.
   - Shows the resolved name they both want.
   - Includes a paste-ready frontmatter snippet:
     ```
     //! [rnme.rename]
     //! name = "<suggested_name>"
     ```
     (Implementor proposes a suggestion heuristic; a plain `<basename>_2` is fine.)
3. The error fires at workspace generation time, before `cargo build` is invoked, so the user sees the error first.

**Acceptance:**
- [ ] Fixture with `foo-bar/RUNME.rs` and `foo_bar/RUNME.rs` siblings, neither renamed, produces the expected error.
- [ ] Same fixture with one of them adding `[rnme.rename] name = "foo_bar_dashed"` builds cleanly.

---

### `test-audit`

**Phase:** 4 (can start during Phase 3)
**Depends on:** `typed-shim-macro` (must be merged for affected tests to manifest); ideal start: during Phase 3
**Assigned to:** `impl-test-audit`
**Validator:** `val-tests`
**Plan approval required:** yes (before mass-modifying tests)

**Description:**

Audit existing tests that construct `TaskContext` directly and either (a) call a task fn directly or (b) rely on body-inline behavior. Update them to either go through the engine path or use a minimal test-only engine helper.

Concrete work:
1. Grep across the workspace for: `TaskContext::new(`, `TaskContext::new_with_buffer(`, and any direct call to a `#[rnme::task]`-annotated fn.
2. Classify each call site:
   - **Body-shape tests** (testing that the body produces certain output for given args): can keep working if they call the renamed private body directly. May need to use the macro-emitted name or a `cfg(test)` re-export.
   - **Behavioral tests** (testing what the task does end-to-end): need to go through `EngineInternals` to construct a real child task.
3. For the behavioral tests, add a small test-only helper in `src/task.rs` (or wherever appropriate) that builds a minimal `TaskContext` with a real `EngineInternals` attached. Suggested signature: `TaskContext::test_with_engine() -> (TaskContext, Arc<EngineInternals>)`.
4. Update each affected test.

The full list of tests to touch is unknown at plan time; the grep step in (1) reveals them. The implementor should produce a short report with the list and proposed treatment for each *before* mass-editing. `lead` approves the plan.

**Acceptance:**
- [ ] `cargo test --workspace` passes.
- [ ] No `#[ignore]` or `#[cfg_attr(not(ci), ignore)]` introduced as a workaround.
- [ ] Audit report lists every affected test and how it was updated.

**Approval gate before coding:** Implementor produces the grep results and proposed treatment per test; `lead` reviews before edits begin.

---

### `fixture`

**Phase:** 2 (start) — runs in parallel with `typed-shim-macro`
**Depends on:** none structurally; uses Phase 1 output as it arrives
**Assigned to:** `impl-fixture`
**Validator:** (covered by `val-typed-shim` / `val-subtasks` / `val-final`)
**Plan approval required:** yes

**Description:**

Build an integration-test fixture that exercises every property the acceptance criteria depend on. Lives alongside `testing/test-tasks/` or in a new sibling crate (implementor proposes).

Concrete work — the fixture should include at minimum:
1. A small RUNME tree, two or three levels deep, with at least one structural-only intermediate dir.
2. Tasks in three arg forms (zero-args, simple-primitives, parser-struct).
3. An in-file typed call (caller and callee in same RUNME.rs).
4. A cross-file typed call (parent calling descendant).
5. A `[rnme.rename]` exercising a normalization-sibling case (`foo-bar` + `foo_bar`).
6. A type export from a child consumed by a parent (`subtasks::child::SomeStruct`).
7. A dynamic-path call (`ctx.run("path:task", &[])`) verifying the dynamic path agrees with the typed path.
8. A negative case: a fixture variant with an unresolved sibling collision that the build is expected to *reject* with the paste-ready error.

The fixture has an associated test driver (a Rust integration test, or a shell script invoked via `cargo test`) that exercises each case and asserts the expected outcome.

**Acceptance:**
- [ ] Fixture is checked in at a known location.
- [ ] All positive cases pass under `cargo test`.
- [ ] Negative case (unresolved collision) produces the expected error and is asserted on.

**Approval gate before coding:** Implementor proposes the fixture layout (paths, RUNME.rs contents, test driver shape) to `lead`. Reason: this fixture is the basis for half the plan's validation; getting its scope right matters.

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
  fixture:
    command: "cargo test -p rnme-test-tasks --test typed_invocation"
    required: true
    description: "Integration tests against the new fixture (path may differ; finalized by `impl-fixture`)."
  manual:
    description: "Spot-check by running rnme against the search_agent RUNME.rs in the working monorepo; confirm `build_wasm(ctx, false, true).spawn()?` works as expected end-to-end."
    required: true
```

## Findings

(Filled in during execution.)

## Decisions Log

(Filled in during execution. Initial entry:)

- 2026-05-18 — Design closed and captured in `docs/invoking_tasks.md`. This plan is the execution of that design; no design decisions remain open at plan-creation time.
- 2026-05-18 — **G1 approved.** Phase 1 lands: `Invocation::{Strings,Factory}` enum in `src/execution/invocation.rs` (FutureFactory is the HRTB `Box<dyn for<'a> FnOnce(&'a TaskContext) -> Pin<Box<dyn Future + Send + 'a>> + Send>`); `__RNME_TASKDEF_<fn_name>` named-static pattern with `TaskDefRef(&'static TaskDef)` newtype submitted to `inventory::collect!(TaskDefRef)`; `Frontmatter.rename: Option<String>` parses `[rnme.rename]` raw (normalization deferred). `Control::SpawnTask` and `EngineHandle::spawn_task` retain their `Vec<String>` external API. Proposals at `docs/plans/proposals/{engine-dispatch,taskdef-static,frontmatter-rename}.md`.
- 2026-05-18 — **Apply-rename design clarifications.**
  - `[rnme.rename]` value is normalized via `heck::to_snake_case` (then the existing path normalizer). New `heck = "0.5"` workspace dep.
  - Rename only applies to children. Per the user: a root file isn't in its own import path, so rename in root frontmatter is structurally inapplicable — not an error. Initial impl rejected root rename with `CompileError::RootRename`; this was reverted in Rev 2.
  - **Rev 2 restructure (user-directed):** rename application moved from flat-per-entry (`process_rnme_file` + `resolved_dir` with `is_root` branch) to tree-traversal (`build_module_tree` + `ModuleNode` recursive type). Normalization only runs inside the child-iteration loop; root is the entry point passed in, never a child of any node. No `is_root` check; no `CompileError::RootRename` variant. The tree is also the data structure `subtasks-injection` (#14) will walk.
- 2026-05-18 — **G2 approved.** Phase 2 lands: typed-shim-macro emits `#[must_use]` builder shims dispatching via `Invocation::Factory` (string wrapper preserved for `ctx.run`); apply-rename Rev 2 with tree-traversal model; integration fixture with 9 live tests + 4 ignored deferred to Phase 3/4. Proposals at `docs/plans/proposals/{typed-shim-macro,apply-rename,fixture}.md`.
- 2026-05-19 — **G3 approved.** Phase 3 lands: `mod subtasks { ... }` emission per parent crate via `MergeNode` walk of the `ModuleNode` tree. Structural-only intermediates emit empty `pub mod` shells; real-dir children emit `pub use ::<crate>::*`. Transitive cargo path-deps added to each parent's Cargo.toml. `debug_assert!` cycle guard on the subtasks dep graph. `CrateEntry` gains `descendant_crate_names: Vec<String>`. Fixture cross-file tests live (12 passed, 1 ignored — collision-negative for #17). Proposal at `docs/plans/proposals/subtasks-injection.md`.
- 2026-05-19 — **G4 approved.** Phase 4 lands: `CompileError::SiblingNameCollision { path_a, path_b, resolved_name, suggestion }` fires at workspace-generation time with paste-ready `[rnme.rename]` snippet; suggestion heuristic uses `_dashed` when one of the two paths has dashes, else `_2`. test-audit closed with zero edits — 12 `TaskContext::new(` sites audited, none required migration. All 13 fixture tests live. Plan complete.

## Blockers

(Filled in during execution.)
