# Test Audit Report — `test-audit` (TaskList #19)

## Scope

Per `2026-05-18-typed-task-invocation.md` §`test-audit`: audit tests that
build `TaskContext` directly and/or call `#[rnme::task]`-annotated fns
directly. Phase 2 (`typed-shim-macro`) has landed: the public ident of
each `#[rnme::task]` fn now resolves to a thin shim returning
`TaskBuilder`; the real async work lives under `__rnme_body_<fn>`. The
wrapper registered in `TaskDef::func` still calls the renamed body via
the same `TaskFn` signature, so `task.func.call(&ctx, args)` is
unchanged in mechanics.

## Baseline

`cargo test --workspace --no-run` compiles cleanly. Per the task brief,
873 tests across the workspace already pass. The audit is about
*post-shim correctness of test intent*, not build breakage.

## Grep results

### `TaskContext::new(` call sites

| # | File:Line | Surrounding test/fn | Classification |
|---|-----------|---------------------|----------------|
| 1 | `tests/integration.rs:472` | `test_stop_all_kills_spawned_processes` | **Body-shape (intentional)** |
| 2 | `src/task.rs:1213` | `Registry::run_with_args` (production code) | **Production runtime** — not a test |
| 3 | `src/task.rs:1314` | `Registry::run_parallel` (production code) | **Production runtime** — not a test |
| 4 | `src/task.rs:1435` | `test_exec_on_context` | **Harness-only** (no task fn involved) |
| 5 | `src/task.rs:1444` | `test_spawn_on_context` | **Harness-only** (no task fn involved) |
| 6 | `src/task.rs:1512` | `test_task_output_returns_output` | **Harness-only** (no task fn involved) |
| 7 | `src/task.rs:1737` | `test_ctx_run_without_registry` | **Harness-only** — exercises the no-registry error path |
| 8 | `src/task.rs:1750` | `test_ctx_tasks_with_registry` | **Harness-only** — exercises registry injection |
| 9 | `src/task.rs:1761` | `test_ctx_tasks_without_registry` | **Harness-only** — exercises no-registry path |
| 10 | `src/execution/handle.rs:404` | `run_unknown_task_errors_at_await` | **Harness — engine present** (engine started above; bare ctx is the test condition) |
| 11 | `src/execution/handle.rs:417` | `run_without_engine_errors_at_await` | **Harness — no engine** (the test name says it: the absence is the point) |
| 12 | `src/execution/execution.rs:394` | `TaskExecution::spawn_body` (production code) | **Production runtime** — not a test |

### `TaskContext::new_with_buffer(` call sites

No matches. The actual constructor with explicit capacity is
`TaskContext::with_capacity`; that has zero call sites in tests. The
exact symbol the plan called out simply does not exist in the codebase.

### Direct calls to `#[rnme::task]`-annotated fns

No matches outside RUNME.rs files. All call sites in `RUNME.rs` files
(e.g. `caller_in_file` → `root_noop(ctx).await?`) are by design — they
exercise the typed shim path the plan introduces.

### Direct `func.call(&ctx, ...)` call sites

| File:Line | Context | Classification |
|-----------|---------|----------------|
| `tests/integration.rs:473` | `test_stop_all_kills_spawned_processes` (paired with site #1 above) | **Body-shape (intentional)** |
| `src/task.rs:1214` | `Registry::run_with_args` (production) | Production runtime |
| `src/execution/execution.rs:462` | `spawn_body` (production) | Production runtime |

## Classification rationale

### Body-shape (intentional) — keep as-is

**`test_stop_all_kills_spawned_processes`** (tests/integration.rs:469-504):
The test asserts that calling `task.func.call(&ctx, &[])` for
`spawn_sleeper` records PGIDs on `ctx.spawned_pgids` and that
`ctx.stop_all(...)` kills those process groups. It deliberately runs
without an engine because the entire surface under test is
`TaskContext::spawned_pgids` + `TaskContext::stop_all` — a property of
`TaskContext` itself, independent of engine dispatch. Going through the
engine would *hide* that property by capturing PGIDs in the child task's
own `TaskContext`, not the test's `ctx`.

Post-shim, `task.func.call(&ctx, &[])` still calls
`__runme_taskfn_spawn_sleeper` which in turn calls
`__rnme_body_spawn_sleeper(ctx)`. The body runs `ctx.spawn("sleep 300")`
which writes to `ctx.spawned_pgids`. Behavior preserved. Test intent
preserved.

The plan's classification of this as "behavioral" (would belong on the
engine path) is, on inspection, incorrect for this specific test —
threading it through the engine would *defeat* what the test is
checking. Keep as-is.

### Harness-only — keep as-is

The `src/task.rs` and `src/execution/handle.rs` test sites all fall
into one of:
- "ctx by itself works" (`test_exec_on_context`, `test_spawn_on_context`,
  `test_task_output_returns_output`) — they don't touch any
  `#[rnme::task]`-annotated fn and don't depend on engine dispatch.
  Constructing a bare `TaskContext` is exactly what these tests need.
- "no registry / no engine" error paths
  (`test_ctx_run_without_registry`, `test_ctx_tasks_without_registry`,
  `run_unknown_task_errors_at_await`, `run_without_engine_errors_at_await`)
  — the bare construction *is* the test setup. Threading them through
  the engine would not be possible.
- "registry injection" sanity (`test_ctx_tasks_with_registry`) — same
  rationale: constructs ctx, injects registry, asserts query surface.

None of these tests would change behavior under the shim because none
of them call a `#[rnme::task]`-annotated public symbol.

### Production runtime — not in scope

`Registry::run_with_args`, `Registry::run_parallel`, and
`TaskExecution::spawn_body` construct `TaskContext::new` as part of the
production runtime path. They are out of scope for a *test* audit.

## Is `TaskContext::test_with_engine()` needed?

**No.** The plan suggested adding `TaskContext::test_with_engine() ->
(TaskContext, Arc<EngineInternals>)` if behavioral tests need an
engine. After classification, zero tests require it:

- The one test that calls `func.call` on a `#[rnme::task]`-annotated
  fn through a bare `TaskContext` (`test_stop_all_kills_spawned_processes`)
  is intentionally bare; the engine would defeat what it tests.
- Every other test that exercises engine-mediated invocation already
  spins up a real `Engine` via `Engine::start(registry)` and dispatches
  through `handle.spawn_task(...)` (see e.g. `test_cross_task_error_propagation`,
  `test_output_capture_from_exec`, `test_discover_and_run_tasks`).

Adding the helper now would be unsolicited scaffolding for zero
callers. Per CLAUDE.md "No Unsolicited Keep-It-Working Work": skip it.

## Proposed edits

**None.**

## Acceptance check

- `cargo test --workspace` already passes (per brief).
- No `#[ignore]` shortcuts proposed.
- Every `TaskContext::new` call site classified and accounted for.

## Decision requested

Confirm:
1. `test_stop_all_kills_spawned_processes` keep-as-is (body-shape test,
   bare-ctx is intentional).
2. No new `TaskContext::test_with_engine` helper added — no caller
   needs it.
3. Task #19 completes with zero code changes.

If lead disagrees on any of (1)-(3), please specify which test(s) to
re-architect and how.
