# Plan: Task Templates

## Goal

Add a library-side `#[rnme::task_template]` proc macro and a consumer-side `rnme::import_task!` declarative macro so external Rust crates (e.g. a hypothetical `rnme-cargo`) can ship reusable typed task definitions that the consumer's RUNME.rs re-stamps as fully-local typed task registrations — running in the consumer's `__RNME_DIR`, under the consumer's `__RNME_GROUP`, reachable via the typed shim surface and `subtasks::...` cross-file machinery. Source of truth: [`docs/task_templates.md`](../task_templates.md).

## Approach

Three independent landings, sequenced by risk:

1. **De-risking spike.** Confirm the proc-macro-emits-`macro_rules!` pattern can do what the design assumes — specifically: a `macro_rules!` exported from a library, invoked at a consumer site, can stamp out a `TaskDef` static that reads `__RNME_GROUP`/`__RNME_DIR` from the consumer's scope, plus a `pub fn` typed shim, plus an `inventory::submit!`. Use a hand-written prototype (no proc macro yet) to validate the end-to-end shape compiles and registers correctly.

2. **`#[rnme::task_template]` proc macro.** New attribute macro in `rnme-macros`. Reuses `detect_arg_form` / `generate_simple_args` / doc-comment extraction from the existing `task` macro. Emits four artifacts at the library site: renamed body fn, string-args wrapper, arg-metadata fn, and a per-task `#[macro_export] macro_rules! __rnme_stamp_<name>!` whose expansion stamps out the consumer-local artifacts. Deliberately emits **no** `TaskDef` static, **no** `inventory::submit!`, **no** `start_task` injection, and **no** typed shim at the library site — those all happen at stamp time.

3. **`rnme::import_task!` proc macro + `#[rnme::task]` hardening.** Add `import_task!` as a function-like proc macro in `rnme-macros` (re-exported as `rnme::import_task!`) that expands `rnme::import_task!(path::to::task);` into `path::to::__rnme_stamp_task!(...);`. Proc-macro form chosen at gate 1 — declarative `macro_rules!` can't synthesize `__rnme_stamp_<task>` from a captured ident. Harden `#[rnme::task]` to fail compile with a pointed error when `__RNME_GROUP`/`__RNME_DIR` aren't in scope.

Each landing has an implementor task + a validator task. The implementation-lead pulls them in order.

## Acceptance Criteria

- A library crate (a new `testing/test-task-templates/` fixture) can declare three task templates covering all three argument forms (zero-arg, simple primitive args, clap parser struct).
- A consumer RUNME.rs can import each template with `rnme::import_task!(rnme_test_task_templates::name);` and:
  - The task appears in `rnme list` under the consumer's group, not the library's.
  - The task's working directory matches the consumer's `__RNME_DIR` (verified by a task that prints `ctx.task_dir()` or runs `pwd`).
  - The task is callable via the typed shim from an ancestor RUNME.rs as `subtasks::consumer_path::name(ctx, ...).await?`.
  - `--help` reflects the template's argument metadata.
- A library-provided bulk `macro_rules! import_all_test_templates!` (written by the fixture using `#[macro_export]` and calling `rnme::import_task!` per task) works from a consumer site with one invocation.
- A typo like `rnme::import_task!(rnme_test_task_templates::buil);` fails to compile with a clear "no rules expected this token" / "cannot find macro `__rnme_stamp_buil`" error sourced from the library path.
- `#[rnme::task]` applied in a regular library crate that does NOT define `__RNME_GROUP`/`__RNME_DIR` fails to compile with a message naming `#[rnme::task_template]` as the correct alternative. Existing `testing/test-tasks/` (which already defines both constants manually) continues to compile.
- `InitContext::register_task` continues to work unchanged. Existing `tests/dynamic_registration.rs` passes.
- Existing test suites pass: `cargo test`, `cargo clippy`, `cargo build`. `tests/typed_invocation.rs` and `tests/shared_tasks.rs` continue to pass.

## Human Review Gates

1. **After spike (Task 1 validator).** User reviews the spike findings before we commit to the proc-macro design. If the `macro_rules!`-stamped-by-proc-macro pattern hits a real Rust limitation, we re-plan rather than working around it. This is the "does this even work" gate.
2. **After `task_template` macro lands (Task 2 validator).** User reviews the emitted token shape and the fixture before we wire up `import_task!`. Cheap to redirect at this point.
3. **Before merging.** Final review of the consumer ergonomics — `import_task!` call sites, error messages on typos, error message on `#[rnme::task]` in a library.

## Team Composition

All teammates spawn inside the existing `megathread-passthrough-tasks` team.

- `implementor-spike` — Opus. Hand-writes a minimal end-to-end prototype (one library crate, one consumer RUNME.rs) that mimics what the macros will eventually emit, without writing the macros. Validates the stamp-at-consumer-site pattern compiles and `rnme list` sees the task.
- `validator-spike` — Sonnet. Verifies the spike's outputs, files a short risks/learnings note for the supervisor to surface to the user at gate 1.
- `implementor-task-template-macro` — Opus. Implements `#[rnme::task_template]` in `rnme-macros`, lifts shared helpers (`detect_arg_form`, arg metadata generation, doc-comment extraction) into a shared module if needed.
- `validator-task-template-macro` — Opus. Builds the `testing/test-task-templates/` fixture, runs `cargo expand` (or equivalent) to verify the emitted token shape, runs `cargo build -p rnme-test-task-templates` and confirms no `TaskDef` static / no `inventory::submit!` are emitted at the library site.
- `implementor-import-task-and-hardening` — Opus. Adds `rnme::import_task!` declarative macro at the crate root, hardens `#[rnme::task]` to fail without `__RNME_GROUP`/`__RNME_DIR`, wires the fixture into a consumer-side integration test mirroring `tests/typed_invocation.rs`.
- `validator-import-task-and-hardening` — Opus. Runs the new integration test, checks the typed shim is reachable from an ancestor RUNME.rs, confirms typo + missing-constants compile errors are sensible, runs the full existing test suite (`cargo test`, `cargo clippy`).

Sonnet is used only for the spike validator (mechanical: did it compile, did `rnme list` show the task). All macro/codegen work is Opus.

## Task Breakdown

### T1 — Spike: hand-rolled stamp-out prototype

**Owner:** `implementor-spike`
**Depends on:** none
**Description:** Without writing any proc-macro changes, build a hand-rolled prototype that exercises the design's central assumption: a library crate exposes a `#[macro_export] macro_rules! __rnme_stamp_demo!` whose expansion, invoked from a consumer RUNME.rs, produces the same artifacts `#[rnme::task]` produces today (`__RNME_TASKDEF_demo` static reading the consumer's `__RNME_GROUP`/`__RNME_DIR`, `inventory::submit!`, typed shim `pub fn demo(...) -> TaskBuilder`). The library also hand-writes `__rnme_body_demo`, `__runme_taskfn_demo`, and `__runme_argmeta_demo` — these are what `task_template` will later auto-generate. Place the library at `testing/test-task-template-spike/` (a new fixture) and the consumer in an existing test RUNME.rs (or a minimal new one). Run `rnme list` and a `rnme run` of the imported task; confirm cwd is the consumer's.

**Acceptance:**
- `cargo build` of the workspace succeeds.
- `rnme list` shows the imported task under the consumer's group.
- The imported task runs in the consumer's directory (verified by printing `ctx.task_dir()`).
- The hand-rolled `macro_rules!` expansion compiles when invoked at the consumer site — specifically, the macro's body references to `__RNME_GROUP` and `__RNME_DIR` resolve at the call site, not the library site.
- Document any surprises (hygiene gotchas, path resolution issues, must-export-also patterns) in a short note for the gate-1 review.

### T1V — Validator: spike

**Owner:** `validator-spike`
**Depends on:** T1
**Description:** Run `cargo build`, `cargo clippy`, `cargo test --test typed_invocation` (sanity), and the spike's specific `rnme list` / `rnme run` checks. File a short report listing what worked, what required hygiene workarounds, and any blockers. **Human review gate 1 fires here.**

### T2 — Implement `#[rnme::task_template]`

**Owner:** `implementor-task-template-macro`
**Depends on:** T1V (and gate 1 approval)
**Description:** Add the `task_template` attribute macro to `rnme-macros/src/lib.rs`. Behavior:

- Accepts the same three argument forms as `#[rnme::task]` (reuse `detect_arg_form`, `generate_simple_args`, doc-comment extraction).
- Emits, at the library site:
  - `fn __rnme_body_<name>(...)` — the user's renamed body, **without** `start_task` injection (start_task is added at stamp time so the task's tracing span carries the consumer's name).
  - `fn __runme_taskfn_<name>(...)` — the string-args wrapper, identical in shape to today's wrapper but calling `__rnme_body_<name>` (which will be in the same module as the stamp call). Pub.
  - `fn __runme_argmeta_<name>() -> Option<clap::Command>` — pub.
  - `#[macro_export] macro_rules! __rnme_stamp_<name>` — emits the per-task helper. Its body, when invoked, stamps:
    - `pub static __RNME_TASKDEF_<name>: TaskDef { ..., group: __RNME_GROUP, dir: __RNME_DIR, func: TaskFnKind::Static(<library-path>::__runme_taskfn_<name>), arg_metadata: <library-path>::__runme_argmeta_<name>, ... }` — captures consumer-site constants by referring to them by bare name inside the macro_rules expansion.
    - `inventory::submit!(TaskDefRef(&__RNME_TASKDEF_<name>))`
    - A `#[must_use] pub fn <name>(ctx: &TaskContext, <typed-params>) -> TaskBuilder` whose factory closure dispatches to `<library-path>::__rnme_body_<name>(body_ctx, <args>)` — same shim shape as today's `#[rnme::task]`.
- Captured ui_hint / description / typed-param list are baked into the `macro_rules!` arm at proc-macro time (the proc macro has full AST access; the declarative macro doesn't need to introspect).
- Does **not** emit a local `TaskDef` static, `inventory::submit!`, or a typed shim at the library site.
- The library-path used in the stamped-out `func` and shim factory must be resolvable at the consumer site. Use `$crate::__runme_taskfn_<name>` etc. inside the `macro_rules!` body so the path roots at the library crate regardless of the consumer's import alias. (Open question, see Risks.)

Re-export `task_template` from `src/lib.rs` and `src/prelude.rs` next to `task`.

**Acceptance:**
- `#[rnme::task_template]` compiles on a function matching each of the three argument forms.
- `cargo expand` (or test by inspection) of a fixture confirms no `TaskDef` static and no `inventory::submit!` are emitted at the library site.
- `__rnme_stamp_<name>!` exists at the library crate root and is referenceable.

### T2V — Validator: `task_template` macro

**Owner:** `validator-task-template-macro`
**Depends on:** T2
**Description:** Build `testing/test-task-templates/` (new fixture) with one template per arg-form. Run `cargo build -p rnme-test-task-templates`; inspect output (`cargo expand` if available); confirm no `inventory::iter::<TaskDefRef>` finds the templates' tasks when the test-task-templates crate is the only `__rnme_link`-called crate. Run `cargo clippy`. **Human review gate 2 fires here.**

### T3 — `rnme::import_task!` + `#[rnme::task]` hardening

**Owner:** `implementor-import-task-and-hardening`
**Depends on:** T2V (and gate 2 approval)
**Description:**

1. Add `import_task!` as a **function-like proc macro** in `rnme-macros`. Re-export from the `rnme` crate root as `rnme::import_task!`. Parses input as a `syn::Path`, splits into library path + final task ident, and expands to `<lib_path>::__rnme_stamp_<task>!();`. The declarative-macro form originally drafted in the design is not viable: T1 confirmed that `macro_rules!` cannot synthesize the ident `__rnme_stamp_<task>` from a captured `$task:ident` (no token-paste in declarative macros). Proc macro chosen by user at gate 1 because `rnme-macros` is already a build-time dep and the proc-macro form preserves the design's preferred call-site syntax `rnme::import_task!(rnme_cargo::build);` and per-task `__rnme_stamp_<name>!` shape from T2.
2. Harden `#[rnme::task]`: at expand time, emit a `const _: &str = __RNME_GROUP; const _: &str = __RNME_DIR;` pair (or equivalent) at the start of the expansion so that the macro fails to compile when those constants aren't in scope. Wrap the failure in a `compile_error!`-with-message expansion if possible (e.g. via a trait-based detection or a `#[cfg(any())]`-gated probe) so the diagnostic names `#[rnme::task_template]` as the alternative. If a clean compile_error path isn't possible, the bare unresolved-const error from the const-reference is acceptable as a fallback — flag this for human review.
3. Add an integration test at `tests/task_templates.rs` mirroring `tests/typed_invocation.rs` / `tests/shared_tasks.rs`: a consumer RUNME.rs imports templates from `rnme-test-task-templates`, runs `rnme list`, invokes the task, and an ancestor RUNME.rs invokes it via the typed shim as `subtasks::child::name(ctx, ...).await?`.
4. Update `docs/task_templates.md`'s status footer to reflect implementation (no design change).

**Acceptance:**
- `rnme::import_task!(rnme_test_task_templates::build);` compiles and registers the task at the consumer site.
- A typo (`buil`) produces a compile error that points at the library path.
- `#[rnme::task]` in a fresh library crate with no `__RNME_GROUP`/`__RNME_DIR` fails to compile; error message includes the substring `task_template`.
- `testing/test-tasks/` (which already defines both constants manually) continues to compile unchanged.
- New integration test passes; full `cargo test` and `cargo clippy` pass.

### T3V — Validator: import_task + hardening

**Owner:** `validator-import-task-and-hardening`
**Depends on:** T3
**Description:** Run `cargo test`, `cargo clippy --all-targets --all-features`, `cargo build`. Confirm the new `tests/task_templates.rs` covers: zero-arg, simple-args, parser-struct template; consumer-local execution (cwd = consumer); cross-file invocation via typed shim. Manually construct a small failing fixture for the `#[rnme::task]` hardening error and confirm the error message names `task_template`. Run a bulk-import library macro from the fixture to confirm the design's "bulk import is just a library helper" claim. **Human review gate 3 fires here.**

## Validation Profile

Build/test/lint commands run by validators:

- `cargo build` (workspace)
- `cargo test` (workspace)
- `cargo test --test task_templates` (new test; T3+)
- `cargo test --test shared_tasks` (regression: existing manually-defined library tasks)
- `cargo test --test typed_invocation` (regression: typed shim path)
- `cargo test --test dynamic_registration` (regression: `register_task` unchanged)
- `cargo clippy --all-targets --all-features -- -D warnings`
- Manual: `rnme list` and `rnme run <imported-task>` against a fixture consumer.

## Risks and Open Questions

- **OPEN: `macro_rules!`-emitted-by-proc-macro path resolution.** The library's stamp helper, when invoked at a consumer site, must emit a `TaskFnKind::Static(...)` that references the library's wrapper fn (`__runme_taskfn_<name>`). Inside a `macro_rules!` exported with `#[macro_export]`, paths starting with `$crate` resolve to the **defining** crate — which is what we want. But the stamp macro also needs to read `__RNME_GROUP` / `__RNME_DIR` from the **caller** site, which works for bare identifiers but not for `$crate`-prefixed paths. The design assumes this asymmetry works; T1 (spike) is the gate that confirms it.
- **RESOLVED (gate 1): `import_task!` is a proc macro, not declarative.** T1 confirmed that `macro_rules!` can do the path-splitting via tt-munching but cannot synthesize the ident `__rnme_stamp_<task>` from a captured `$task:ident` (no token-paste in declarative macros). The plan's two-arg fallback doesn't escape this either — splitting the args doesn't help with the ident-concat. User chose the proc-macro form (option 3 in the gate-1 surface) because `rnme-macros` is already a build-time dep, the cost is minimal, and the proc-macro form preserves the design's preferred call-site syntax and the per-task `__rnme_stamp_<name>!` shape from T2. T3 implements `import_task!` as a `syn::Path`-parsing function-like proc macro.
- **OPEN: hardening error message UX.** Emitting a `compile_error!` from `#[rnme::task]` only when `__RNME_GROUP`/`__RNME_DIR` aren't in scope requires detecting absence at proc-macro time, which the proc macro can't do (it doesn't see the surrounding scope). Workarounds: emit a `const _: &str = __RNME_GROUP;` that errors with "cannot find value `__RNME_GROUP`" (worse UX, no `task_template` hint), or emit a trait bound that fails with a custom message (better UX, more codegen). The plan should accept whichever works; surface to user at gate 3 if the UX is bare.
- **NON-RISK (called out for clarity):** The existing `testing/test-tasks/` library crate defines `__RNME_GROUP`/`__RNME_DIR` manually and uses `#[rnme::task]`. The hardening check is "constants in scope", not "is this a RUNME.rs". So that fixture continues to compile. The design's prose says the hardening makes `#[rnme::task]` "incompatible with library use" — that's overstated; it's incompatible with library use that doesn't manually define the constants. This is fine.
- **OPEN: `start_task` injection site.** Current `#[rnme::task]` injects `let _task = ctx.start_task(<name>);` into the body. For templates, the name is determined at stamp time (could be different from the template's body name, in principle — though the design says the consumer uses the same name). The plan injects `start_task` at stamp time, not at body-emit time. T2 confirms this works without doubling the start_task call.
