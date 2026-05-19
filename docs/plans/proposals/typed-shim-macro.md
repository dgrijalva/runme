# Typed Shim Macro — Proposal

**Task:** `typed-shim-macro` (Phase 2, plan §252)
**Author:** `impl-typed-shim-macro`
**Status:** awaiting G2 review
**Scope:** Rewrite `#[rnme::task]` so the user-written body moves to a private symbol and the user-facing name becomes a thin shim that returns a `#[must_use]` `TaskBuilder` configured with an `Invocation::Factory(...)` and a reference to the named `__RNME_TASKDEF_<fn>` static.

This proposal commits to the *exact* emitted shape per arg form and to one new constructor on `TaskBuilder`. The dynamic string-args path (`__runme_taskfn_<name>`) is preserved verbatim — only the new shim is added.

---

## 1. Anatomy of the rewrite

Today (`macros/src/lib.rs:486–506`) the macro emits:

```
#input_fn          // user's async fn at the original name
#wrapper           // __runme_taskfn_<name>: the TaskFn pointer
#arg_metadata      // __runme_argmeta_<name>: clap::Command builder
pub static __RNME_TASKDEF_<name>: TaskDef = TaskDef { func: TaskFnKind::Static(__runme_taskfn_<name>), ... };
inventory::submit! { TaskDefRef(&__RNME_TASKDEF_<name>) }
```

The change: rename `#input_fn`'s identifier in place (`build_wasm` → `__rnme_body_build_wasm`), keep its full body (including the injected `let _task = ctx.start_task(...)`), and emit one new public shim fn at the *original* name with the same parameter list but a `TaskBuilder` return type.

Everything else — the wrapper, arg-metadata, static, inventory submission — is left structurally untouched. The wrapper continues to call the *renamed* body fn instead of the original name. That's the only change inside the existing emit blocks.

---

## 2. Exact emitted shim per arg form

For all three forms the shim:

1. Captures typed args by value (move).
2. Builds a `FutureFactory` closure that, given a child `&TaskContext`, calls `__rnme_body_<fn>` with the captured args and `Box::pin`s the resulting future.
3. Hands the factory and `&__RNME_TASKDEF_<fn>` to `TaskBuilder::from_factory(ctx, &TASKDEF, factory)`.

`ctx` (the first parameter) is *not* captured in the factory closure — the closure receives a *fresh* child-task `&TaskContext` from the engine. The shim only reads from the caller's `ctx` to pluck `parent_id` and `engine` for the builder.

### Form 1: zero-args

User wrote:
```rust
/// Build the project
#[rnme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo build").await?.ok()?;
    Ok(())
}
```

Emitted:
```rust
// Renamed body — private to the crate.
async fn __rnme_body_build(ctx: &::rnme::task::TaskContext) -> ::rnme::error::TaskResult {
    let _task = ctx.start_task("build");
    ctx.exec("cargo build").await?.ok()?;
    Ok(())
}

// User-facing shim at the original name.
#[must_use = "task builders do nothing until `.await` or `.spawn()` — \
              a bare call constructs the builder and drops it"]
pub fn build(ctx: &::rnme::task::TaskContext) -> ::rnme::execution::builder::TaskBuilder {
    ::rnme::execution::builder::TaskBuilder::from_factory(
        ctx,
        &__RNME_TASKDEF_build,
        ::std::boxed::Box::new(move |body_ctx| {
            ::std::boxed::Box::pin(__rnme_body_build(body_ctx))
        }),
    )
}

// String wrapper, arg_metadata, static, inventory — unchanged structure,
// but the wrapper now calls __rnme_body_build instead of build.
fn __runme_taskfn_build<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ... {
    ::std::boxed::Box::pin(async move { __rnme_body_build(ctx).await })
}
fn __runme_argmeta_build() -> Option<::rnme::clap::Command> { None }
#[allow(non_upper_case_globals)]
pub static __RNME_TASKDEF_build: ::rnme::task::TaskDef = ::rnme::task::TaskDef {
    name: "build",
    description: Some("Build the project"),
    group: __RNME_GROUP,
    func: ::rnme::task::TaskFnKind::Static(__runme_taskfn_build),
    arg_metadata: __runme_argmeta_build,
    ui_hint: None,
};
::rnme::inventory::submit! { ::rnme::task::TaskDefRef(&__RNME_TASKDEF_build) }
```

### Form 2: simple primitive args

User wrote:
```rust
/// Build the WASM target
#[rnme::task]
async fn build_wasm(ctx: &TaskContext, release: bool, watch: bool) -> TaskResult {
    /* body */
    Ok(())
}
```

Emitted (only the body + shim are new-shaped; the rest mirrors Form 1):
```rust
async fn __rnme_body_build_wasm(
    ctx: &::rnme::task::TaskContext,
    release: bool,
    watch: bool,
) -> ::rnme::error::TaskResult {
    let _task = ctx.start_task("build_wasm");
    /* body */
    Ok(())
}

#[must_use = "..."]
pub fn build_wasm(
    ctx: &::rnme::task::TaskContext,
    release: bool,
    watch: bool,
) -> ::rnme::execution::builder::TaskBuilder {
    ::rnme::execution::builder::TaskBuilder::from_factory(
        ctx,
        &__RNME_TASKDEF_build_wasm,
        ::std::boxed::Box::new(move |body_ctx| {
            ::std::boxed::Box::pin(__rnme_body_build_wasm(body_ctx, release, watch))
        }),
    )
}
```

Note: `release` and `watch` are `bool` — `Copy`, so the `move` closure captures them by value trivially. For `String`, `Vec<T>`, `Option<T>` (the other primitives recognized by `classify_param`), the same `move` works — they are `Send + 'static` owned values.

### Form 3: parser struct

User wrote:
```rust
#[derive(clap::Parser)]
pub struct DeployArgs {
    #[arg(long)]
    env: String,
    #[arg(long)]
    dry_run: bool,
}

/// Deploy to an environment
#[rnme::task]
async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult {
    /* body */
    Ok(())
}
```

Emitted shim:
```rust
async fn __rnme_body_deploy(
    ctx: &::rnme::task::TaskContext,
    args: DeployArgs,
) -> ::rnme::error::TaskResult {
    let _task = ctx.start_task("deploy");
    /* body */
    Ok(())
}

#[must_use = "..."]
pub fn deploy(
    ctx: &::rnme::task::TaskContext,
    args: DeployArgs,
) -> ::rnme::execution::builder::TaskBuilder {
    ::rnme::execution::builder::TaskBuilder::from_factory(
        ctx,
        &__RNME_TASKDEF_deploy,
        ::std::boxed::Box::new(move |body_ctx| {
            ::std::boxed::Box::pin(__rnme_body_deploy(body_ctx, args))
        }),
    )
}
```

The struct moves into the closure by value. No `Clone`, no `Copy` required — the closure is `FnOnce` and the value is consumed by the single call to `__rnme_body_deploy`.

---

## 3. Closure capture strategy & trait bounds

The `FutureFactory` type from Phase 1 (`src/execution/invocation.rs:28`) is:

```rust
pub type FutureFactory = Box<
    dyn for<'a> FnOnce(&'a TaskContext) -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>>
        + Send,
>;
```

The shim's emitted closure shape:

```rust
Box::new(move |body_ctx: &TaskContext| -> Pin<Box<dyn Future<Output = TaskResult> + Send + '_>> {
    Box::pin(__rnme_body_<fn>(body_ctx, <captured-args>))
})
```

Satisfies the `FutureFactory` bounds because:

- **`FnOnce`**: each `Invocation::Factory` is consumed exactly once at `spawn_body`. The `move` keyword forces capture-by-value; the body call consumes the captured args.
- **`Send` on the closure**: all captured args are `Send` by virtue of being the same types the user's `async fn` takes — those types must already be `Send` for the body's future to be `Send` (a requirement that already exists today since `TaskFn` returns a `Send` future).
- **`Send` on the returned future**: `__rnme_body_<fn>` is an `async fn` whose desugaring is `Send` iff all locals are `Send`. Same constraint as before — no change.
- **`for<'a> FnOnce(&'a TaskContext) -> ... + 'a`**: the closure does not name `'a`. The HRTB is automatic when no captured value borrows the argument; the captured args are all owned, so the closure satisfies `for<'a> FnOnce(&'a _)`. The returned future borrows `body_ctx` through the body fn (`__rnme_body_<fn>` is `async fn`, so its future borrows its `&TaskContext` arg for `'a`).

**No `Clone` bound on Form-3 args.** The closure is `FnOnce`. `move` captures by value, and `__rnme_body_<fn>(body_ctx, args)` consumes the captured value exactly once. The arg type only needs the `Send` already required by the existing body's `Send` future.

**Caveat to confirm with reviewer (open question 1):** if the user's body fn has `Send` issues today, the wrapper's `Box<dyn Future<...> + Send>` would already fail. The shim doesn't make this strictly tighter, but it does add a *second* call site that propagates the `Send` requirement through a `Box<dyn FnOnce ... + Send>`. I don't expect this to break any existing tasks, but flagging because it's a non-trivial trait bound being added to every emitted shim.

---

## 4. Metadata routing (`mode`, doc-description)

Unchanged from today. Both pieces live on `TaskDef`, not on the shim:

- **`mode = cli|tui`** continues to be parsed at `macros/src/lib.rs:230-279` and emitted into `__RNME_TASKDEF_<fn>.ui_hint`. The shim does not carry or expose `ui_hint`.
- **Doc comments** are still collected at `macros/src/lib.rs:310-332` and emitted into `__RNME_TASKDEF_<fn>.description`. The shim does not carry a description.

In other words: every existing line of metadata-routing code in `macros/src/lib.rs` (lines 220–338) stays. The only change inside the existing code blocks is that `#input_fn`'s ident is replaced before re-emission, so the renamed body fn is what actually appears in the output token stream.

---

## 5. `TaskBuilder` construction

`TaskBuilder` today (`src/execution/builder.rs:29-108`) has two constructors:
- `pub(crate) fn failed(err: TaskError) -> Self`
- `pub(crate) fn new(parent_id, engine, task_def, args: Vec<String>) -> Self`

The shim needs a third entry point that takes a `FutureFactory` instead of `args`. **Proposed addition** to `src/execution/builder.rs`:

```rust
/// Build a TaskBuilder for the typed-shim path emitted by
/// `#[rnme::task]`. Resolves `parent_id` and `engine` from the caller's
/// `TaskContext` the same way `TaskContext::run` does, and stages the
/// factory as `Invocation::Factory` so it bypasses `task.func` at
/// `spawn_body` time.
///
/// Visibility is `pub` (not `pub(crate)`) because this is invoked from
/// the macro-emitted code, which lives in user crates.
#[must_use = "task builders do nothing until `.await` or `.spawn()`"]
pub fn from_factory(
    ctx: &crate::task::TaskContext,
    task_def: &'static TaskDef,
    factory: crate::execution::invocation::FutureFactory,
) -> Self {
    let Some(engine) = ctx.engine_weak() else {
        return Self::failed(TaskError::from_display("no engine context"));
    };
    Self {
        inner: Ok(TaskBuilderInner {
            parent_id: ctx.task_id(),
            engine,
            task_def,
            invocation_kind: InvocationKind::Factory(factory),
            timeout: None,
        }),
    }
}
```

This requires two follow-on tweaks inside `builder.rs`:

1. **Replace `inner.args: Vec<String>` with `inner.invocation_kind: InvocationKind`** where:
   ```rust
   enum InvocationKind {
       Strings(Vec<String>),
       Factory(FutureFactory),
   }
   ```
   `InvocationKind::Strings` matches today's storage; `InvocationKind::Factory` is the new path. `TaskBuilder::spawn` then converts `InvocationKind` → `Invocation` at the `spawn_child` call:
   ```rust
   let invocation = match inner.invocation_kind {
       InvocationKind::Strings(a) => Invocation::Strings(a),
       InvocationKind::Factory(f) => Invocation::Factory(f),
   };
   engine.spawn_child(parent_id, inner.task_def, invocation, opts)
   ```

   Rationale for the internal enum (rather than holding `Invocation` directly on `TaskBuilderInner`): `Invocation::Factory` is non-`Clone` and non-`Debug`. The builder also carries `Result<Inner, TaskError>` so we want the storage to remain `move`-friendly. Storing the same shape works.

2. **`TaskBuilder::new`** (today the string-args constructor for `ctx.run`) is updated to wrap its `args` as `InvocationKind::Strings(args)`. One-line change.

3. **`engine_weak()`** accessor on `TaskContext` — does this exist? `TaskContext::engine_internals()` exists (`src/task.rs:802`) but it `upgrade()`s. For the shim we need a `Weak` because the builder stores `Weak<EngineInternals>`. Either:
   - **(a)** Add a `pub fn engine_weak(&self) -> Option<Weak<EngineInternals>>` accessor on `TaskContext`. Returns the field directly (it's already `Option<Weak<...>>` internally).
   - **(b)** Have `TaskBuilder::from_factory` accept `Option<Weak<EngineInternals>>` directly and let the macro emit `ctx.engine_weak()` — same outcome.

   I'll go with (a). Cleaner separation: macro emits a single call into `TaskBuilder::from_factory(ctx, ...)`, and the accessor stays inside `task.rs` next to the field.

4. **`#[must_use]` on `TaskBuilder` and `SpawnBuilder`.** Add `#[must_use = "..."]` to both type declarations. `SpawnBuilder` lives at `src/process/spawn_builder.rs` (or wherever — I'll grep for it before code). The lint message reads: `"task builders do nothing until \`.await\` or \`.spawn()\` — a bare call constructs the builder and drops it"` for `TaskBuilder`, and an analogous message for `SpawnBuilder`.

---

## 6. `ctx.run("name", &[...])` path preservation

The dynamic string path stays. Every emitted item in §2's "unchanged" rows is identical to today's output, including:

- `__runme_taskfn_<fn>` (the `TaskFn` pointer used by `Invocation::Strings` dispatch).
- `__runme_argmeta_<fn>` (clap-command builder for `TaskInfo::args_help` and CLI help).
- `__RNME_TASKDEF_<fn>` static with `func: TaskFnKind::Static(__runme_taskfn_<fn>)`.
- `inventory::submit! { TaskDefRef(&__RNME_TASKDEF_<fn>) }`.

Only difference inside `__runme_taskfn_<fn>`: today it calls `<fn_name>(ctx, ...)`; after the rewrite it calls `__rnme_body_<fn_name>(ctx, ...)`. This is a one-token substitution in `generate_simple_args` and the Form-1/Form-3 call expressions.

Concretely, the `fn_call` builder at `macros/src/lib.rs:387-433` currently does e.g. `quote! { #fn_name(ctx) }`. After the change: it emits `quote! { #renamed_body_name(ctx) }`. `__rnme_body_<fn_name>` is the new ident.

Result: `ctx.run("build_wasm", &["--release", "--watch"]).await?` resolves via the registry to `__RNME_TASKDEF_build_wasm`, which dispatches `Invocation::Strings` through `TaskFnKind::Static(__runme_taskfn_build_wasm)`, which clap-parses and then calls `__rnme_body_build_wasm(ctx, release, watch).await`. Same body executes, same framework integration, same logs.

Both paths converge on the same body. ✓

---

## 7. Files touched

| File | Change |
|---|---|
| `macros/src/lib.rs` | Rename `#input_fn`'s ident in place; emit new public shim per arg form; update `fn_call` to call the renamed body; (no other emit changes). |
| `src/execution/builder.rs` | Replace `args: Vec<String>` field with `invocation_kind: InvocationKind`; add `pub fn from_factory(...)`; add `#[must_use]` on the type; convert `InvocationKind` → `Invocation` at `spawn`. |
| `src/task.rs` | Add `pub fn engine_weak(&self) -> Option<Weak<EngineInternals>>` accessor. |
| `src/process/spawn_builder.rs` (or wherever `SpawnBuilder` is defined) | Add `#[must_use]`. |

No new files. No new modules. The macro change is one ident substitution + one new emit block per arg form. The builder change is one internal enum + one new constructor.

---

## 8. What this task does *not* do

- No removal of `__runme_taskfn_<fn>`, `__runme_argmeta_<fn>`, `__RNME_TASKDEF_<fn>`, or `TaskFnKind::Static`. All four stay.
- No `subtasks::...` codegen. That's `subtasks-injection` (Phase 3).
- No `[rnme.rename]` plumbing. That's `apply-rename` (Phase 2, parallel).
- No restart-of-Factory handling. Same `OPEN` as the engine-dispatch proposal §5 — deferred.
- No changes to `TaskFn`, `DynamicTaskFn`, or `TaskFnKind` (per `taskdef-static` proposal's same boundary).
- No changes to tests in this task — `test-audit` (Phase 4) handles any tests broken by the body-rename. The note in plan §282 explicitly delegates this.

---

## 9. Open questions for the reviewer

Numbered for easy reply:

1. **`Send` propagation in shim closure.** The shim's `Box<dyn FnOnce(&TaskContext) -> ... + Send>` adds a `Send` requirement on the *captured-args tuple* (in addition to the future). For Form-2 primitives and Form-3 structs people are likely to write, this is satisfied. But if a user writes a Form-3 task with a non-`Send` arg today, the failure mode shifts from "future not Send at body" to "closure not Send at shim". Acceptable, or should I add a clearer error path? My read: leave it — rustc's error message is fine, and a non-`Send` task is broken either way.
2. **`engine_weak()` accessor placement.** Add it as `pub fn engine_weak` on `TaskContext` per §5.3.(a)? Or fold the engine-resolution into `TaskBuilder::from_factory` as `from_factory(ctx, ...)` with `ctx.engine_internals().map(Arc::downgrade)`? The latter avoids exposing a new public accessor on `TaskContext`. I'm inclined toward exposing it (cleaner separation), but flagging.
3. **`must_use` message wording.** Current proposal: `"task builders do nothing until \`.await\` or \`.spawn()\` — a bare call constructs the builder and drops it"`. Open to a shorter message.
4. **Renamed body symbol visibility.** `async fn __rnme_body_<fn>` — `pub(crate)` or no visibility (private to module)? The shim is in the same module, so private suffices. I'll emit no visibility modifier (private). Confirm.
5. **Macro-emitted symbol name collision.** `__rnme_body_<fn>` joins `__runme_taskfn_<fn>`, `__runme_argmeta_<fn>`, `__RNME_TASKDEF_<fn>` as a reserved prefix. Plenty of headroom; flagging only because it expands the set of names a user can't define adjacent to a task.
