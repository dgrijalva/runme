# Engine Dispatch — Proposal

**Task:** `engine-dispatch` (Phase 1, plan §147)
**Author:** `impl-engine-dispatch`
**Status:** awaiting G1 review
**Scope:** Add a future-factory invocation path through the engine alongside the existing `Vec<String>` args path. Both produce identical `TaskHandle`s and converge on `EngineInternals::spawn_child`.

This proposal commits to the type signatures and dispatch shape so `typed-shim-macro` (Phase 2) can codegen against a stable contract.

---

## 1. Types

Lives in **`src/execution/invocation.rs`** (new file), re-exported from `src/execution/mod.rs`.

```rust
use std::future::Future;
use std::pin::Pin;

use crate::error::TaskResult;
use crate::task::TaskContext;

/// A factory that, given a `&TaskContext` for the freshly-registered
/// child task, produces the boxed future that *is* the task body.
///
/// The HRTB on the `&'a TaskContext` argument lets the closure be stored
/// without naming a lifetime — the engine produces the `TaskContext`
/// inside `spawn_body` after the node is registered, then hands a
/// borrow to the factory to construct the future against that borrow.
/// The returned future's lifetime is tied to the borrow, matching the
/// `task.func.call(&body_ctx, &args)` shape already used today.
///
/// `Send` on the closure and the returned future is required because
/// the body runs under `tokio::spawn`. `FnOnce` because each
/// `Invocation::Factory` is consumed exactly once when `spawn_body`
/// runs.
pub type FutureFactory = Box<
    dyn for<'a> FnOnce(&'a TaskContext)
            -> Pin<Box<dyn Future<Output = TaskResult> + Send + 'a>>
        + Send,
>;

/// How a task body is invoked at `spawn_body` dispatch time.
///
/// `Strings` is today's path: the engine calls `task_def.func` with
/// stringified args. `Factory` is the new typed path: the engine calls
/// the closure (which captured typed args in the caller's stack frame
/// and resolved them to the body symbol) and awaits the future it
/// returns. Both variants funnel through `EngineInternals::spawn_child`
/// → `TaskExecution::spawn_body`.
pub enum Invocation {
    Strings(Vec<String>),
    Factory(FutureFactory),
}
```

Notes on signature choices:

- **`for<'a> FnOnce(&'a TaskContext) -> Pin<Box<dyn Future<...> + Send + 'a>>`** mirrors the existing `TaskFn` type (`src/task.rs:63`). The HRTB makes the factory storable as a `Box<dyn ...>` without naming the lifetime; the closure is invoked once `spawn_body` has a `TaskContext` to borrow from.
- **No `Sync`.** Factories are consumed once on the body's tokio task and never shared. `FnOnce + Send` matches `tokio::spawn`'s requirements.
- **`TaskResult` (= `Result<(), TaskError>`)** matches the body's return type today, so the dispatch site can treat both variants identically downstream.
- **No `'static` bound on the closure.** The HRTB on `&TaskContext` handles the borrow lifetime; the closure itself is owned by `Invocation::Factory` and lives for as long as the `Invocation` does (i.e., until consumed by `spawn_body`).

---

## 2. Touched files / signatures

### `src/execution/invocation.rs` (new)
Defines `FutureFactory` and `Invocation` as above.

### `src/execution/mod.rs`
Add `pub mod invocation;` and re-export `Invocation`, `FutureFactory`.

### `src/execution/engine.rs:390` — `EngineInternals::spawn_child`

Today:
```rust
pub fn spawn_child(
    self: &Arc<Self>,
    parent_id: TaskId,
    def: &'static TaskDef,
    args: Vec<String>,
    opts: SpawnOptions,
) -> Result<TaskHandle, TaskError>
```

New:
```rust
pub fn spawn_child(
    self: &Arc<Self>,
    parent_id: TaskId,
    def: &'static TaskDef,
    invocation: Invocation,
    opts: SpawnOptions,
) -> Result<TaskHandle, TaskError>
```

The `args: Vec<String>` parameter is replaced wholesale by `invocation: Invocation`. The body change is mechanical: where the current code passes `args` into `spawn_body`, it now passes `invocation` through unchanged.

### `src/execution/execution.rs:358` — `TaskExecution::spawn_body`

Today's signature:
```rust
pub fn spawn_body(
    &mut self,
    self_weak: Weak<TaskExecution>,
    engine: Weak<EngineInternals>,
    task: &'static TaskDef,
    task_args: Vec<String>,
)
```

New signature:
```rust
pub fn spawn_body(
    &mut self,
    self_weak: Weak<TaskExecution>,
    engine: Weak<EngineInternals>,
    task: &'static TaskDef,
    invocation: Invocation,
)
```

`self.task_args: Vec<String>` is preserved on `TaskExecution` for the **restart** path (`root.rs:153` re-spawns via the saved args), but it is now only populated when `invocation` is `Invocation::Strings(_)`. When the invocation is a `Factory`, `self.task_args` stays `Vec::new()`. See §5 for the restart implication and how it stays inside this task's scope.

### `src/execution/builder.rs:100` — `TaskBuilder::spawn`

Today:
```rust
engine.spawn_child(parent_id, inner.task_def, inner.args, opts)
```

New (Phase 1 of this task):
```rust
engine.spawn_child(parent_id, inner.task_def, Invocation::Strings(inner.args), opts)
```

The `TaskBuilder` itself remains string-args-only in this phase. The Phase 2 `typed-shim-macro` work extends `TaskBuilder` to also carry a `FutureFactory`; **that extension is not part of this task** and is not pre-built here (per the user's "no scaffolding for future steps" rule).

### `src/execution/root.rs:93` & `:165` — root control loop callers

Two call sites, both passing `args: Vec<String>` today. Both wrap:

```rust
engine.spawn_child(TaskId::ROOT, def, Invocation::Strings(args), opts)
```

### `src/execution/engine.rs:723` — `EngineHandle::spawn_task` / `EngineSpawnBuilder`

The `EngineSpawnBuilder` (line 723) currently carries `args: Vec<String>` and ultimately funnels into `spawn_child` via the `Control::SpawnTask` message. The `Control::SpawnTask` payload (`src/execution/control.rs:169`) carries the same `args` field. We have two options here:

- **(a)** Change `Control::SpawnTask` and `EngineSpawnBuilder` to carry `Invocation` end-to-end.
- **(b)** Keep both at `Vec<String>` (external API), wrap into `Invocation::Strings` only at the `spawn_child` call site in `root.rs`.

I'll go with **(b)**. The external `EngineHandle::spawn_task` API is the entry point from frontends (CLI, MCP, TUI) — all of which only ever produce string args, since they originate from the dynamic path. Carrying `Invocation` across an async channel adds nothing for them and would force every external caller to construct an `Invocation::Strings(args)`. The wrapping happens at the two `root.rs` call sites listed above.

**OPEN:** Reviewer can flip this to (a) if you'd prefer one type all the way down. Flagging because the design doc is silent on it.

---

## 3. Dispatch site — `spawn_body` (execution.rs:442)

The current body is one branch:

```rust
let result = async move { task.func.call(&body_ctx, &task_args).await }
    .instrument(span)
    .await;
```

New shape — symmetric match producing the same `result: TaskResult`:

```rust
let result = match invocation {
    Invocation::Strings(args_owned) => {
        async move { task.func.call(&body_ctx, &args_owned).await }
            .instrument(span)
            .await
    }
    Invocation::Factory(factory) => {
        async move { factory(&body_ctx).await }
            .instrument(span)
            .await
    }
};
```

Properties:
- **Both branches use the same `body_ctx`.** All `TaskContext` setup (set_engine, set_log_store, set_task_identity, set_seq_gen, tracing forwarder, output forwarder, etc.) happens **before** the match. Both variants see an identical `body_ctx`.
- **Both branches are wrapped in `.instrument(span)`** so tracing-via-task-span behaves identically.
- **Both branches return `TaskResult`** and feed the same post-match block (terminal status write, `terminal_override` honor, `ended_at` set, `publish_snapshot`).
- **The `move` captures `task_args` (Strings) or `factory` (Factory) into the async block.** The captured value is consumed exactly once per body run.
- **`task.func.call(...)` is unchanged** for the Strings path. The Factory path bypasses `task.func` entirely — by construction, the factory closure (emitted by `typed-shim-macro`) calls the *renamed private body symbol* directly with typed args.

Error/cancel handling is unchanged: the terminal status writer downstream of the match treats `Ok(())` and `Err(_)` identically regardless of which branch produced it.

---

## 4. Caller migration list

Every existing call site of `spawn_child` and every callsite of `spawn_body`:

| File:line | Call site | Action |
|---|---|---|
| `src/execution/builder.rs:100` | `engine.spawn_child(parent_id, inner.task_def, inner.args, opts)` | Wrap: `Invocation::Strings(inner.args)` |
| `src/execution/root.rs:93` | `engine.spawn_child(TaskId::ROOT, def, args, opts)` (Control::SpawnTask handler) | Wrap: `Invocation::Strings(args)` |
| `src/execution/root.rs:165` | `engine_spawn.spawn_child(TaskId::ROOT, def, args, opts)` (Restart hard path) | Wrap: `Invocation::Strings(args)` |
| `src/execution/engine.rs` `spawn_body` call inside `spawn_child` (line ~419) | `e.spawn_body(self_weak.clone(), engine_weak.clone(), def, args)` | Pass `invocation` through |
| Engine tests in `engine.rs` (e.g. `spawn_task_runs_to_completion` line 1288 area) | Various `spawn_child(...)` and `spawn_task(...)` calls | Wrap each `args` argument in `Invocation::Strings(_)` where it calls `spawn_child` directly; `EngineHandle::spawn_task` keeps its `Vec<String>` external signature per §2 option (b), so those tests are unchanged. |

No new callers are added. No `Invocation::Factory` constructor is invoked from any call site in this task — that's `typed-shim-macro`'s job in Phase 2.

---

## 5. Implication: restart path and `Invocation::Factory`

The hard-restart path at `root.rs:153` re-spawns by cloning `exec.task_args`. With the dispatch change:

- For tasks invoked via `Invocation::Strings`, `task_args` is populated and restart works as it does today.
- For tasks invoked via `Invocation::Factory`, `task_args` is empty (the typed args were captured by the closure, which has been consumed). A hard restart of such a task **cannot reconstruct the typed-args closure** — that closure was per-invocation.

**Behavior in this task:** restart of a Factory-invoked task will re-spawn with empty args, going through the *string* path via `task.func.call(&ctx, &[])`. For most static tasks the body's clap parser will then either use defaults or surface a parse error. This is observable as a regression for the hypothetical case "user typed-invokes a task, then hard-restarts it from the UI" — but that combination doesn't exist in the codebase today (typed invocation is the very thing being added in this plan).

**Decision:** I am not adding restart-of-Factory plumbing in this task. The plan doesn't call for it. The acceptance criteria here are "no behavior change for existing callers, tests pass unchanged" — existing callers all go through `Invocation::Strings`, so restart works identically today. The user's CLAUDE.md is explicit that we don't add scaffolding for future scenarios.

**OPEN:** If reviewer disagrees and wants restart-of-Factory addressed now (or wants an explicit error rather than silent empty-args), flag it. Otherwise this is deferred — `test-audit` or a later task can revisit if it becomes a real call site.

---

## 6. What this task does *not* do

Explicitly out of scope per the plan:

- No changes to `TaskBuilder` shape beyond the one-line wrap at `spawn`.
- No `Factory` constructor on `TaskBuilder`. Phase 2 (`typed-shim-macro`) adds that.
- No new `cmd!`-style macros, no new public API.
- No changes to `TaskDef`, `TaskFn`, `TaskFnKind`. The static-function-pointer path stays. (That's `taskdef-static`'s job to change.)
- No changes to dynamic-task registration (`InitContext::register_task`).

---

## 7. Open questions for the reviewer

Numbered for easy reply:

1. **§2 option (a) vs (b):** carry `Invocation` end-to-end through `Control::SpawnTask` and `EngineSpawnBuilder`, or wrap at the `root.rs` call site? I picked (b); confirm or flip.
2. **§5 restart-of-Factory:** defer (current proposal) or address now with an explicit error? I picked defer.
3. **Naming:** `Invocation` vs `InvocationMode` vs keeping the plan's verbatim `Invocation`. I kept `Invocation`. Same for `FutureFactory`. Confirm or rename.
4. **File location:** new `src/execution/invocation.rs`, or fold the types into `src/execution/control.rs` (which already holds `SpawnOptions`)? I picked the new file because `Invocation` is conceptually about *how the body is dispatched*, not about *spawn configuration knobs*.
