# Task Templates

A mechanism for declaring `#[rnme::task]`-shaped tasks in a regular Rust library crate and re-stamping them as real, locally-owned task registrations at the consumer's RUNME.rs site. The motivating use cases are wrappers around external CLI tools (cargo, npm, kubectl, etc.) that should be reusable across projects without copy-pasting per RUNME.rs.

## Motivation

Today, sharing tasks across RUNME.rs files has two paths and both have defects in this context:

- **Statically declared `#[rnme::task]` in a child RUNME.rs.** Reachable from ancestors via the `subtasks::` module tree (see `invoking_tasks.md`). But the task's `__RNME_DIR` is the child's directory — i.e. the task runs in the *child*'s working directory. That semantic is right for cross-RUNME calls but wrong for "import this from a library and run it locally," where the task should run in the *consumer's* directory.
- **`InitContext::register_task` at the consumer site.** Allows the consumer's `__RNME_DIR` to be used, but produces a `TaskFnKind::Dynamic` task that is reachable only through the string-keyed `ctx.run("name", &args)` API. The typed-call work (`invoking_tasks.md`) is bypassed: no typed shim, no `subtasks::` reachability, no compile-time errors on rename.

Truly-dynamic registration (where the set of tasks or their args is computed at runtime) belongs on the dynamic path and stays there. This document covers the more common case: **statically definable shared tasks** that should be reusable as proper typed registrations at the consumer site.

## Use cases

1. Opt-in tasks shipped from rnme itself or from sibling utility crates.
2. Standalone task-generation libraries published to crates.io (e.g. a hypothetical `rnme-cargo`).
3. Team-internal reusable task libraries.
4. Wrapping any CLI tool so it shows up in the TUI/MCP/etc. (lightweight; may live in one RUNME.rs).

All four use cases want the same shape: "declare a task in regular Rust, stamp it out at the consumer site."

## Mental model

**Library tasks are templates; the consumer site stamps them into real local task definitions.** The library doesn't try to be a usable task on its own — it has no `__RNME_GROUP`, no `__RNME_DIR`, and submits nothing to `inventory`. At the consumer site, an `import_task!` macro invocation produces the full set of artifacts a local `#[rnme::task]` would have emitted (typed shim, string-args wrapper, arg-metadata fn, `TaskDef` static, `inventory::submit!`) — using the **consumer's** `__RNME_GROUP` and `__RNME_DIR`.

Consequences of this model:

- A library-imported task runs in the consumer's directory (correct for the library-wrapper case).
- It appears under the consumer's group.
- It is reachable from the consumer's typed-shim surface and from any ancestor's `subtasks::consumer_path::task_name(...)`.
- Renames in the library are compile-time visible at the consumer site (because the consumer names the task in the macro invocation).

## Design

### `#[rnme::task_template]` — the library-side macro

A new attribute macro distinct from `#[rnme::task]`. Applied to a function with the same signature shape as a task body:

```rust
// In a library crate (e.g. rnme-cargo):
use rnme::prelude::*;

#[derive(clap::Parser)]
pub struct BuildOpts {
    #[arg(long)]
    pub release: bool,
    #[arg(long)]
    pub package: Option<String>,
}

/// Build the current cargo project.
#[rnme::task_template]
pub async fn build(ctx: &TaskContext, opts: BuildOpts) -> TaskResult {
    let release = opts.release.then_some("--release");
    let package = opts.package.as_deref().map(|p| ["--package", p]);
    ctx.exec(cmd!(cargo build {release...} {package...})).await?.ok()?;
    Ok(())
}
```

`#[rnme::task_template]` emits **only** what's needed to re-stamp at the consumer site:

- A `pub` body fn under a private name (e.g. `__rnme_body_build`) carrying the user's code.
- A `pub` string-args wrapper (`__runme_taskfn_build`) — same as today's `#[rnme::task]` produces.
- A `pub` arg-metadata fn (`__runme_argmeta_build`) — same as today.
- A per-task `pub macro_rules!` helper (e.g. `__rnme_stamp_build!`) that, when invoked, expands into the stamp-out artifacts. The helper captures the typed parameter list (because it's emitted from the same proc-macro invocation that has full AST access), so the consumer-side macro doesn't need to introspect it.
- Any metadata the `#[rnme::task]` form carries (description from doc comments, `ui_hint`, etc.) is captured in the helper macro's expansion.

It deliberately **does not** emit:

- A `pub static __RNME_TASKDEF_build` — no local TaskDef is meaningful at the library site.
- An `inventory::submit!` — there's nothing to register at the library site.
- A `start_task` injection in the body — that happens in the stamped-out version at the consumer site.
- A `#[must_use] pub fn build(...) -> TaskBuilder` shim — that shim is produced at the consumer site.

### `rnme::import_task!` — the consumer-side macro

A new **proc macro** provided by `rnme` (lives in `rnme-macros`, which is already a build-time dep of `rnme`, so the cost is minimal). At the consumer's RUNME.rs:

```rust
// In a RUNME.rs:
rnme::import_task!(rnme_cargo::build);
rnme::import_task!(rnme_cargo::test);
```

Each invocation expands into a single call into the library's per-task helper:

```rust
rnme_cargo::__rnme_stamp_build!(/* args identifying consumer site */);
```

**Why a proc macro (not `macro_rules!`).** `import_task!` needs to take a path like `rnme_cargo::build` and dispatch to a per-task helper at `rnme_cargo::__rnme_stamp_build!`. Synthesizing the identifier `__rnme_stamp_build` from `build` requires token pasting, which `macro_rules!` cannot do. A proc macro can construct the helper ident trivially. The call-site syntax and the `__rnme_stamp_<name>!` shape emitted by `#[rnme::task_template]` are unchanged.

The library's helper macro then stamps out, at the consumer site:

- A `pub static __RNME_TASKDEF_build: TaskDef = TaskDef { group: __RNME_GROUP, dir: __RNME_DIR, ..., func: TaskFnKind::Static(<consumer-local wrapper>) };` — using the consumer's `__RNME_GROUP` and `__RNME_DIR`.
- A consumer-local string-args wrapper that delegates to the library's `__runme_taskfn_build` (or directly references it — the wrapper exists primarily so the `TaskFn` pointer in the local `TaskDef` is unambiguously local).
- The `inventory::submit!(TaskDefRef(&__RNME_TASKDEF_build))` so the consumer's binary registers it.
- A `#[must_use] pub fn build(ctx: &TaskContext, opts: BuildOpts) -> TaskBuilder` typed shim that returns a `TaskBuilder::from_factory(...)` whose factory closure dispatches to the library's body fn. The shim has the same signature as the library's `task_template` fn, baked in by the library's macro at the point it was generated.

After expansion, the consumer's RUNME.rs has, for each `import_task!` invocation, the exact same artifacts a local `#[rnme::task]` would have produced — sourced from the library.

### Properties this gives us

- **Typed cross-file invocation works.** The typed shim is a `pub fn build` at the consumer crate's root. Any ancestor RUNME.rs reaches it as `subtasks::consumer_path::build(ctx, opts).await?`. `invoking_tasks.md`'s cross-file machinery is unaware of how `build` got there.
- **Working directory follows the consumer.** `__RNME_DIR` in the stamped-out `TaskDef` is the consumer's. The library's checkout location doesn't matter.
- **Renames at the library are surfaced at the consumer.** `import_task!(rnme_cargo::buil);` (typo) is a compile error.
- **Bulk import is just a library-provided convenience macro.** A library author writes their own `import_*_tasks!` that calls `rnme::import_task!` for each task it wants to bundle:

  ```rust
  // In rnme-cargo:
  #[macro_export]
  macro_rules! import_cargo_tasks {
      () => {
          rnme::import_task!($crate::build);
          rnme::import_task!($crate::test);
          rnme::import_task!($crate::check);
          // ...
      }
  }
  ```

  The consumer writes one line: `rnme_cargo::import_cargo_tasks!();`. No special primitive needed from rnme.

### `#[rnme::task]` becomes incompatible with library use

To prevent footguns where a library author reaches for `#[rnme::task]` (thinking it's the right tool) and produces a crate that pollutes any consumer's `inventory` with tasks carrying garbage `__RNME_GROUP`/`__RNME_DIR`:

`#[rnme::task]` will fail to compile unless `__RNME_GROUP` and `__RNME_DIR` are defined as `const` items in scope. They always are inside RUNME.rs files (the codegen injects them), and never in regular library crates. The error message names the correct macro for library use:

> `#[rnme::task]` requires `__RNME_GROUP` and `__RNME_DIR` constants in scope — these are injected automatically inside RUNME.rs files. To declare a task in a regular Rust library, use `#[rnme::task_template]` instead and have the consumer site re-stamp it with `rnme::import_task!`.

This guardrail is small but load-bearing: it makes the "wrong macro" failure mode self-correcting.

### The dynamic path is unchanged

`InitContext::register_task` remains exactly as it is today. The truly-dynamic case (runtime decides what tasks to register and/or what args they take) is its job, and the existing API handles it. Dynamic tasks continue to be reachable only via the string-keyed path; that's accepted and not a target of this work.

### Argument forms

`#[rnme::task_template]` accepts the same three argument forms `#[rnme::task]` does today:

- Form 1: zero extra params after `ctx`.
- Form 2: simple primitive params, auto-clap-generated.
- Form 3: single non-primitive param implementing `clap::Parser`.

The library author writes whichever fits. The captured signature flows through the per-task helper macro into the consumer-side typed shim.

## Non-goals

- **Subsuming `#[rnme::task]`.** The two macros are deliberately separate. `task_template` is the right choice for items in regular Rust crates intended for re-stamping; `task` is the right choice inside a RUNME.rs.
- **Reading library-side `use` statements to auto-discover what to import.** Rust macros cannot enumerate items in another crate's module. Consumer-side imports are explicit per task or done via library-provided bulk macros.
- **An init-side `ctx.import_task(...)`.** If a consumer wants conditional registration, they should use the existing dynamic `InitContext::register_task` API. The static path is unconditional by design.
- **Per-task fluent builders, named-arg call shapes, etc.** Same call-site conventions as the existing typed-shim work (`invoking_tasks.md`'s "Possible future enhancements" applies equally here).

## Open questions

None at time of writing — see "Settled decisions" below for the questions that were raised during design and the answers.

## Settled decisions

- `#[rnme::task_template]` is a new, separate macro from `#[rnme::task]`. Not a flag or attribute on the existing macro.
- Consumer-side import shape is `rnme::import_task!(path::to::task);` — explicit path, one per task. Bulk import is achieved via library-provided helper macros, not via a built-in.
- Bulk import macros are written by the library author using `macro_rules!` and exported with `#[macro_export]`.
- `#[rnme::task]` is hardened to fail at compile time when `__RNME_GROUP`/`__RNME_DIR` aren't in scope, with an error pointing the author at `#[rnme::task_template]`.
- The `InitContext::register_task` dynamic API is unchanged and remains the path for truly-dynamic registration.
