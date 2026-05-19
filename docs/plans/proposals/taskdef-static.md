# Proposal — `taskdef-static`

**Task:** Phase-1 task #2 of `2026-05-18-typed-task-invocation.md`.
**Author:** `impl-taskdef-static`
**Status:** pending lead approval

## Goal

`#[rnme::task]` should emit each `TaskDef` as a named `static` referenceable
by symbol from the typed shim emitted in Phase 2. Inventory registration is
preserved but switches from "submit a value literal" to "submit a reference
to the named static" so the same `TaskDef` instance is both inventory-visible
and statically nameable from generated code.

## Naming convention

```
__RNME_TASKDEF_<fn_name>
```

where `<fn_name>` is the user's task fn identifier verbatim (no normalization,
no group prefix). Examples:

- `#[rnme::task] async fn build_wasm(...)` → `__RNME_TASKDEF_build_wasm`
- `#[rnme::task] async fn deploy(...)` → `__RNME_TASKDEF_deploy`

**Why this form (and not `__RNME_TASKDEF_<group>_<name>`):**

- Each RUNME.rs becomes its own generated lib crate. The static lives at
  the lib crate's root, so the symbol only needs to be unique within that
  crate. The task fn name is already unique within the crate (Rust enforces
  no-duplicate-fn-names at the same module level), so the static derived
  from it is automatically unique.
- The typed shim emitted in Phase 2 lives in the same module as the static,
  so it references it as a local item path. No group qualification needed
  at the symbol level — the group key is still stored *inside* the
  `TaskDef` value.
- Keeping the symbol short and predictable makes it easier to reason about
  the generated code and easier for `impl-typed-shim-macro` to reference
  by `concat_idents`-style construction.

**Collision risk:** none beyond what Rust's own duplicate-item rules
already catch. Two `#[rnme::task]`-annotated fns named `foo` in the same
RUNME.rs already collide on `fn foo` itself before any macro expansion
considerations apply.

## Visibility

```rust
pub static __RNME_TASKDEF_<name>: ::rnme::task::TaskDef = ...;
```

**Why `pub`:**

- Phase 3 (`subtasks-injection`) generates parent crates that do
  `pub use ::child_crate::*;`. For the typed shim emitted in Phase 2 to
  resolve from a parent crate as `subtasks::child::__RNME_TASKDEF_foo`
  (if it ever needs to), the static must be `pub`.
- Even if no parent-crate reference is needed, `pub(crate)` would forbid
  cross-crate reference, which would foreclose future flexibility for
  zero benefit. The `__RNME_` prefix already signals "internal, don't
  touch".
- The user-visible API surface is what `pub use ::child_crate::*` exposes;
  the `__RNME_` prefix excludes these from any reasonable IDE
  autocompletion at the call site. Functional pub, social-pub: hidden.

**Not in the rnme crate's public surface:** consistent with the
team-lead's directive ("Don't add public re-exports of these statics from
the rnme crate"). The statics live in the *generated* lib crates, not in
the `rnme` library crate itself.

## Inventory mechanics

The current `inventory::submit! { TaskDef { ... } }` expansion (per
`inventory-0.3.24/src/lib.rs:511`) wraps the submitted expression in:

```rust
static __INVENTORY: Node = Node {
    value: &{ TaskDef { ... } },
    ...
};
```

I.e. inventory takes the *value*, constructs a temporary inside the
static-initializer's block expression, then takes its address. The
collected iter then yields `&'static T` where `T` is the collection type
(here, `TaskDef`).

If we naively change the submit expression to `&__RNME_TASKDEF_foo`, the
collection type seen by inventory becomes `&'static TaskDef`, not
`TaskDef`. Concretely:

```rust
inventory::collect!(TaskDef);                // unchanged
inventory::submit! { &__RNME_TASKDEF_foo }   // submits a &TaskDef, not a TaskDef
```

…would compile-error because `T` in `Collect for T` is `TaskDef`, and the
submit block's `&{ &__RNME_TASKDEF_foo }` yields a `& &TaskDef`, which
isn't what the registered collector expects.

### Resolution: introduce a thin newtype submission target

```rust
// in src/task.rs
pub struct TaskDefRef(pub &'static TaskDef);

unsafe impl Send for TaskDefRef {}
unsafe impl Sync for TaskDefRef {}

inventory::collect!(TaskDefRef);
```

Remove `inventory::collect!(TaskDef);` — the collection target changes
from `TaskDef` to `TaskDefRef`. The macro then emits:

```rust
pub static __RNME_TASKDEF_<name>: ::rnme::task::TaskDef = TaskDef { ... };

::rnme::inventory::submit! {
    ::rnme::task::TaskDefRef(&__RNME_TASKDEF_<name>)
}
```

`Registry::from_inventory` updates to iterate `TaskDefRef` and unwrap:

```rust
pub fn from_inventory() -> Self {
    let mut reg = Self::new();
    for r in inventory::iter::<TaskDefRef> {
        reg.tasks.push(r.0);
    }
    reg
}
```

`Registry`'s internal `Vec<&'static TaskDef>` is unchanged; only the
inventory iteration step changes shape. Every existing consumer of
`Registry` keeps working unchanged.

**Why a newtype rather than reusing the existing `TaskDef`:**

- inventory collects by type. We need a stable target type whose
  collected value is a *reference* into the rest of the program rather
  than an inlined value. `&'static TaskDef` is not itself a struct type
  we can `impl Collect for`. A newtype is the simplest stable wrapper.
- Memory cost: one `Node` per task (already paid) plus one `TaskDefRef`
  (16 bytes: a fat-pointer-free `&'static TaskDef`) per task. Negligible.
- The `TaskDef` itself is unduplicated — the named static is the single
  storage location, referenced by both inventory and the typed shim.

### Tests in `task.rs` (lines 1345+, 1503+)

These define `static TEST_TASK_A: TaskDef = TaskDef { ... };` and feed
them to `Registry::register(&TEST_TASK_A)` *without* going through
inventory. They do not use `submit!`, so they are unaffected by this
change. The newtype type only matters where `inventory::submit!` is
called, which is exclusively inside the macros.

## Symbol-collision protection

Two cases:

1. **Two tasks with the same `name` inside one RUNME.rs.** Rust already
   rejects this at the `fn` level (`fn foo` declared twice → compile
   error). The static name derives from the fn name, so collisions
   never reach the static-generation step.

2. **Two tasks with the same `name` in different RUNME.rs files.** Each
   RUNME.rs becomes its own lib crate, so the statics are in different
   crate-level namespaces. No symbol collision at the linker level. The
   inventory `Node` symbols generated by `inventory::submit!` use
   hygienic anonymous names (`__INVENTORY` inside a `const _: () = { ... }`
   block), so those are also unique-per-submit-site.

Within the same crate, then, the only collision risk is the user
declaring two `#[rnme::task]` fns named the same — already a compile
error before the macro emits anything. **No new collision class is
introduced by this change.**

## Generated-code diff

### Before

```rust
// macros/src/lib.rs, expansion of #[rnme::task] async fn build_wasm(...)
#input_fn       // injected start_task + user body
#wrapper        // __runme_taskfn_build_wasm
#arg_metadata_tokens

::rnme::inventory::submit! {
    ::rnme::task::TaskDef {
        name: "build_wasm",
        description: Some("..."),
        group: __RNME_GROUP,
        func: ::rnme::task::TaskFnKind::Static(__runme_taskfn_build_wasm),
        arg_metadata: __runme_argmeta_build_wasm,
        ui_hint: None,
    }
}
```

### After

```rust
// macros/src/lib.rs, expansion of #[rnme::task] async fn build_wasm(...)
#input_fn
#wrapper
#arg_metadata_tokens

pub static __RNME_TASKDEF_build_wasm: ::rnme::task::TaskDef = ::rnme::task::TaskDef {
    name: "build_wasm",
    description: Some("..."),
    group: __RNME_GROUP,
    func: ::rnme::task::TaskFnKind::Static(__runme_taskfn_build_wasm),
    arg_metadata: __runme_argmeta_build_wasm,
    ui_hint: None,
};

::rnme::inventory::submit! {
    ::rnme::task::TaskDefRef(&__RNME_TASKDEF_build_wasm)
}
```

Two notable points:

1. The `TaskDef` value moves from inline-inside-`submit!` to a named
   `pub static`. The `submit!` site shrinks to a one-liner wrapping a
   reference.
2. `inventory::collect!` target changes from `TaskDef` to `TaskDefRef`,
   and `Registry::from_inventory` adjusts its iteration accordingly.
   No public API of `Registry` changes.

## Out of scope

- The typed shim itself (Phase 2 task `typed-shim-macro`).
- Any change to `TaskFnKind`, `TaskFn`, the `ArgMetadataFn` typedef, or
  the wrapper fn (`__runme_taskfn_<name>`) — all unchanged.
- Dynamic-path tasks registered via `InitContext::register_task` — they
  continue to use `Arc<TaskDef>` (or whatever the current dynamic path
  is); no inventory involvement.
- Removing the `unsafe impl Send/Sync for TaskDef` — still needed
  because `TaskDef` is referenced from inventory via the new newtype,
  and the newtype's safety claim transitively depends on `TaskDef` being
  `Send + Sync`.

## Concrete files touched

- `src/task.rs` — add `pub struct TaskDefRef(pub &'static TaskDef);`,
  add `unsafe impl Send/Sync` for it, change
  `inventory::collect!(TaskDef);` to `inventory::collect!(TaskDefRef);`,
  update `Registry::from_inventory` body.
- `macros/src/lib.rs` — modify the `#[rnme::task]` expansion at line 481
  per the diff above. The static name uses `quote!`'s `Ident`
  construction:
  ```rust
  let static_name = syn::Ident::new(
      &format!("__RNME_TASKDEF_{}", fn_name),
      fn_name.span(),
  );
  ```

No other files touched. No changes to the codegen pipeline
(`src/bin/rnme/`), the engine, the TUI, or any test crate.

## Acceptance verification

- `cargo build` clean.
- `cargo test --workspace` clean (existing tests don't reference the
  inventory target type by name; they go through `Registry::register`).
- Manual: build a single-file RUNME.rs containing one `#[rnme::task]`
  fn, expand with `cargo expand`, confirm the emitted `pub static
  __RNME_TASKDEF_<name>: TaskDef` appears and the `submit!` block
  references it.
