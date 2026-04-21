# Process Execution Refactor: Spawn Unification, Readiness, Log Streams

## Context

The task runner needs three capabilities before MCP/agent integration (Phase 8): declarative readiness conditions, richer task/process handles, and log stream access from task code. These are tightly connected — an agent needs to start a task, know when it's ready, and stream its logs.

Today `exec` and `spawn` are two separate code paths with different buffering strategies (sync `&mut OutputBuffer` vs async `Arc<Mutex<OutputBuffer>>`), different observability (spawn emits `SpawnEvent` for sidebar, exec doesn't), and different return types. Readiness (`TaskStatus::Ready`) is defined but never set. Log streams exist internally (`OutputBuffer`, `LogStore`, broadcast channels) but aren't accessible from task code or external consumers.

**Goal:** Unify exec/spawn around a single `spawn` primitive with a builder API, add declarative readiness probes, and expose log streams through handles — so the following workflow works:

```rust
#[task]
async fn integration_test(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo build").await.ok()?;
    let srv = ctx.spawn("./server").ready_on_port(8080).await?;
    srv.wait_ready().await?;
    ctx.bind_ready(&srv);
    ctx.exec("cargo test --test integration").await.ok()?;
    srv.stop(Duration::from_secs(5)).await?;
    Ok(())
}
```

## Phase 1: Termination Enum

Replace bare `exit_code: i32` with a richer termination model.

**`crates/runme/src/process.rs`:**
- Add `Termination { Exited(i32), Signaled(Signal), TimedOut }` with `success()`, `exit_code()`, `Display`
- Change `ProcessResult.exit_code` field to `termination: Termination`
- Update `ProcessResult::success()`, `exit_code()` to delegate; add `termination()` accessor
- Update construction sites in `exec()` (~line 410) and `ProcessHandle::wait()` (~line 267) to detect signals via `ExitStatusExt::signal()`
- Add `ProcessHandle::complete(self) -> ProcessResult` as alias for `wait()` (prep for Phase 4)

**`crates/runme/src/error.rs`:**
- Update `From<ProcessResult> for TaskError` to use `Termination` for richer messages (signaled → exit 128+sig, timed out → exit 124)

**`crates/runme/src/execution.rs`:**
- Update `ProcessStatus::Failed(i32)` to `ProcessStatus::Failed(Termination)` for sidebar display

**`crates/runme/src/tui/sidebar.rs`:**
- Update status display: `Failed(Exited(code))` → "FAIL:{code}", `Failed(Signaled(sig))` → "SIG:{sig}", `Failed(TimedOut)` → "TIMEOUT"

**`crates/runme/src/prelude.rs`:** Export `Termination`.

## Phase 2: SpawnBuilder + IntoFuture

Make `ctx.spawn(cmd)` return a `SpawnBuilder`. Existing `.await?` call sites work unchanged via `IntoFuture`.

**`crates/runme/src/process.rs`:**
- Add `SpawnBuilder` struct: `{ cmd, task_name, buffer, timeout, ready_timeout, readiness, on_spawn }` (readiness fields are `Option`s, populated in Phase 5)
- `on_spawn: Option<Box<dyn FnOnce(&ProcessHandle) + Send>>` — callback for post-spawn work (pgid tracking, SpawnEvent emission)
- Builder methods: `timeout()`, `ready_timeout()` (store values, enforce in Phase 5)
- `IntoFuture for SpawnBuilder` → calls existing `process::spawn()`, then `on_spawn(&handle)`, returns `Result<ProcessHandle, ProcessError>`
- `SpawnBuilder::complete(self) -> CompletionFuture` → spawns, immediately waits, returns `Result<ProcessResult, ProcessError>`
- `CompletionFuture` — newtype wrapping `Pin<Box<dyn Future<...> + Send>>`, impl `Future`

**`crates/runme/src/task.rs`:**
- `TaskContext::spawn()` changes from `async fn` to `fn` returning `SpawnBuilder`
- The current post-spawn logic (pgid tracking, SpawnEvent emission) moves into an `on_spawn` closure captured by the builder
- All existing `ctx.spawn(cmd).await?` sites compile unchanged

## Phase 3: exec() as spawn().complete()

Eliminate the separate `process::exec()` code path.

**`crates/runme/src/task.rs`:**
- `TaskContext::exec()` becomes: `self.spawn(command).complete().await`
- Every exec'd process now gets its own buffer, emits a `SpawnEvent`, appears in the sidebar — solving the open issue about exec visibility
- Source name changes from `task_name` to `command_label` (each exec'd command is a distinct source in the log viewer)

**`crates/runme/src/process.rs`:**
- Remove `process::exec()` function and the sync `drain_records()` helper (only used by exec)
- Keep `drain_records_async()` (used by spawn's background tasks)

**`crates/runme/src/execution.rs`:**
- Remove `start_buffer_forwarder()` for exec output buffer — no longer needed since exec output flows through spawn's per-process buffer → SpawnEvent → monitor_spawns → LogStore

**Note:** `TaskContext::output` field remains for `println()` and tracing fallback. It's not used by exec anymore.

## Phase 4: ProcessHandle / ProcessResult Shared Surface

Ensure both types expose the same data.

**`crates/runme/src/process.rs`:**
- `ProcessHandle::complete(self) -> ProcessResult` consumes the handle (replaces `wait(&mut self)`)
- Remove `wait()` (no backward compat concerns)
- Add `command_label` field to both `ProcessHandle` and `ProcessResult`
- Add `label(&self) -> &str` method on both
- `ProcessResult` constructed by `complete()` carries label, output, termination from the handle

## Phase 5: Readiness Conditions

**`crates/runme/src/process.rs`:**
- Add `ReadinessCondition` enum: `Port(u16)`, `Http(String)`, `Custom(Box<dyn ...>)`
- Builder methods on `SpawnBuilder`: `ready_on_port(u16)`, `ready_on_http(impl Into<String>)`, `ready_when(async closure)`
- Add readiness state to `ProcessHandle`: `readiness_rx: tokio::sync::watch::Receiver<bool>`
- When `IntoFuture` resolves and readiness is configured: spawn a background tokio task that polls the condition (TCP connect for port, HTTP GET for http, await for custom), sends `true` on the `watch::Sender` when satisfied
- `ready_timeout`: wraps probe in `tokio::time::timeout`; on expiry, probe exits without setting ready
- `timeout`: spawns a background task that kills the process after the duration
- `ProcessHandle::wait_ready(&self)` — `self.readiness_rx.clone().wait_for(|&r| r).await`
- `ProcessHandle::is_ready(&self) -> bool` — `*self.readiness_rx.borrow()`

**`crates/runme/src/task.rs`:**
- Add `task_status: Option<Arc<Mutex<TaskStatus>>>` field to `TaskContext`
- `ctx.bind_ready(&handle)` — spawns background task: await handle's readiness, set TaskStatus::Ready
- `ctx.mark_ready()` — directly sets TaskStatus::Ready

**`crates/runme/src/execution.rs`:**
- `TaskExecution::launch()` passes `task_status` Arc to TaskContext
- Update `SpawnEvent` to carry `readiness_rx: Option<watch::Receiver<bool>>`
- `monitor_spawns()` stores readiness state in `ProcessInfo`; spawns watcher to update `ProcessInfo.ready` field
- Add `ready: bool` field to `ProcessInfo`

**`crates/runme/src/tui/sidebar.rs`:**
- Show readiness indicator on processes that have a readiness condition

## Phase 6: LogStore.output()

Let LogStore produce `Output` handles for external consumption.

**`crates/runme/src/log/store.rs`:**
- `LogStore::output(&self) -> Output` — create fresh `OutputBuffer`, copy all entries, spawn forwarding task from LogStore broadcast, return `Output` wrapping the buffer
- `LogStore::output_for(&self, source: &str) -> Output` — same but filtered by source name (backfill + forward only matching entries)

This enables task handles to expose log access: a parent task that runs `ctx.run("web").spawn()` can access the child task's LogStore and produce per-source or unified Outputs.

## Verification

After each phase, run `cargo build && cargo test` from workspace root. Key test areas:

- **Phase 1:** Existing process tests pass with updated exit_code API. Signaled process test verifies `Termination::Signaled`.
- **Phase 2:** Existing `ctx.spawn(cmd).await?` call sites compile. New SpawnBuilder tests cover `.await` and `.complete()`.
- **Phase 3:** Existing exec tests pass through new code path. Sidebar now shows exec'd processes.
- **Phase 4:** `handle.complete()` tests verify ownership transfer and data preservation.
- **Phase 5:** Integration test: spawn a process that opens a port after a delay, `wait_ready()` completes. `bind_ready` sets TaskStatus::Ready.
- **Phase 6:** `LogStore::output()` snapshot correctness + live forwarding.

## Critical Files

| File | Changes |
|------|---------|
| `crates/runme/src/process.rs` | Termination, SpawnBuilder, IntoFuture, readiness probes, complete() |
| `crates/runme/src/task.rs` | spawn() returns builder, exec() as sugar, bind_ready, mark_ready |
| `crates/runme/src/execution.rs` | ProcessStatus/ProcessInfo updates, SpawnEvent changes, TaskStatus wiring |
| `crates/runme/src/error.rs` | From<ProcessResult> for TaskError uses Termination |
| `crates/runme/src/log/store.rs` | output() and output_for() methods |
| `crates/runme/src/tui/sidebar.rs` | Termination display, readiness indicator |
| `crates/runme/src/prelude.rs` | Export new types |
