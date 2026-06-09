# Research: Cancellation Primitive

## Recommendation

**Use `tokio_util::sync::CancellationToken` as the core cancellation primitive.**

## Candidates Evaluated

### 1. tokio_util::sync::CancellationToken (RECOMMENDED)
- Parent-child token tree with `.child_token()` for hierarchical cancellation
- Cooperative: cancellation propagates only at `.await` points
- Cheap signaling via `is_cancelled()` checks and `.cancelled().await` waits
- No built-in handling of resource cleanup (Drop impl is user-written)
- Not yet in Cargo.toml dependencies; would require adding `tokio-util`

### 2. tokio::task::JoinHandle::abort()
- Preemptive cancellation at tokio task boundaries
- **Problem: Does NOT propagate through user task code** — only cancels at the next `.await` boundary in the tokio runtime machinery itself, not in user futures
- **Problem for parent-child**: Children spawned with `tokio::spawn()` don't inherit cancellation semantics; sibling `.abort()` won't propagate to grandchildren
- **Problem: Doesn't compose with process killing** — tasks manage subprocesses via signals (SIGTERM/SIGKILL); JoinHandle::abort has no connection to that machinery
- **Gotcha**: Aborting a handle that's already awaited is a no-op; doesn't work with "Drop without awaiting" semantics

### 3. Custom signaling via tokio::sync::watch
- Watch channel allows pushing a cancellation boolean across task boundaries
- Cooperative, integrates with existing code (already used for readiness probes in `SpawnEvent`)
- **Problem: No parent-child composition built-in** — would require manually wiring parent tokens to children
- **Problem: No resource cleanup hook** — Drop impl must manually signal the channel, error-prone
- **Less robust**: Requires developers to wire cancellation checks explicitly; easy to forget

### 4. Custom signaling via tokio::sync::oneshot
- One-time notification trigger; minimal state
- **Problem: Cancels only once, no re-triggering** — incompatible with engine receiving multiple `KillTask` messages for the same task
- **Problem: No parent-child composition** — manual wiring required
- **Worse than watch**: More limited use case

## Existing Patterns in Runme

The codebase already handles process shutdown cleanly:
- **`TaskExecution::shutdown()`** uses SIGTERM → timeout → SIGKILL on OS-level process groups
- **`ProcessHandle::stop()`** mirrors this gracefully
- **`TaskContext::stop_all()`** broadcasts shutdown to all spawned process groups

Task lifetime currently managed via:
- `tokio::task::JoinHandle` for the task function (held in `TaskExecution::task_handle`)
- RAII cleanup in `TaskExecution` (processes cleaned up on drop)
- `LaunchConfig` hooks for TUI-layer lifecycle signals

## Why CancellationToken

1. **Cooperative cancellation through user code** — propagates only at `.await` points, letting user tasks react cleanly
2. **Parent-child token trees compose naturally** — each child task gets `.child_token()` from parent; cancelling parent flows to all descendants
3. **Cheap Drop-based cancellation** — call `token.cancel()` in `TaskHandle::Drop`, no lock contention
4. **Compatible with existing process killing** — CancellationToken cancels the task; the task's finally blocks invoke `ctx.stop_all()` to clean up spawned processes
5. **Detached children work correctly** — `tokio::spawn(async move { ctx.run(...).await })` gets its own token branch; if parent cancels, the spawned task observes the token but isn't forcibly aborted
6. **Engine-side KillTask reuses the same path** — both `TaskHandle::Drop` and `Engine::KillTask` call `token.cancel()`

## Parent-Child Wiring Sketch

```rust
pub struct TaskExecution {
    // ... existing fields ...
    cancellation_token: CancellationToken,
    parent_token: Option<CancellationToken>,  // None for root
}

impl TaskExecution {
    pub fn new_child(parent_token: CancellationToken) -> Self {
        let token = parent_token.child_token();  // Automatic parent-child link
        Self {
            cancellation_token: token,
            parent_token: Some(parent_token),
            // ...
        }
    }
}

pub struct TaskHandle {
    cancellation_token: CancellationToken,
    // ... other fields (IntoFuture, etc.) ...
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.cancellation_token.cancel();  // Cheap, fires regardless of await
    }
}

// Inside task body (via ctx.run_with_registry or similar):
pub async fn run_with_cancellation(
    task_fn: TaskFn,
    ctx: TaskContext,
    cancellation_token: CancellationToken,
) -> Result<(), TaskError> {
    tokio::select! {
        result = task_fn(&ctx, args) => result,
        _ = cancellation_token.cancelled() => {
            ctx.stop_all(timeout).await;
            Err(TaskError::Cancelled)
        }
    }
}
```

## Key Wiring Points for Architect

1. **`TaskExecution` constructor** — holds a `CancellationToken`; if parented, use `.child_token()`
2. **`ctx.run(name, args)` return path** — wrap the result in `TaskHandle { cancellation_token: token.clone() }`
3. **Task body execution** — wrap the task function's future in a `tokio::select!` on `cancellation_token.cancelled()`
4. **`Engine::KillTask` handler** — look up task's `CancellationToken` by `TaskId`, call `.cancel()`
5. **Process cleanup on cancellation** — leverage existing `ctx.stop_all()` pattern; call it from the task's error handler or finally block

## Gotchas

1. **CancellationToken not yet in Cargo.toml** — requires adding `tokio-util` dependency. Check version compatibility with the current tokio version (1.x).

2. **Cancellation is cooperative** — if user task code has an infinite loop with no `.await`, cancellation won't interrupt it. This is intentional (matches Rust's cancellation safety model). Tasks that `ctx.spawn(heavy_cpu_task)` with no checkpoints won't react. Document this clearly.

3. **Parent cancellation is transitive** — if the root task is cancelled, ALL descendants cancel automatically. The engine's `Control::Quit` path must differentiate between "cancel this task" and "shut down the entire runtime" (the latter cancels root, the former cancels a child). Design the control protocol to be explicit about scope.

4. **Detached tasks (tokio::spawn(async move { ctx.run(...) })) don't auto-cancel with parent** — they get a child token, but if they're spawned *before* the parent cancels, they keep running until their token fires. This is correct RAII behavior. Make sure the design clarifies: a detached child's lifetime is bound to the spawned tokio task, not the parent's synchronous scope.

5. **No built-in timeout on cancellation** — if a task doesn't respond to cancellation within a deadline, there's no automatic SIGKILL equivalent. For now, tasks are trusted to cooperate. If future enforcement is needed, layer it on top: cancel the token, then spawn a watcher that forcibly aborts the tokio task if the token is still observed after N seconds.

6. **Drop impl on TaskHandle must be infallible** — `CancellationToken::cancel()` is infallible, so this is fine. But if future refactors add logging or async cleanup to Drop, be very careful (async Drop doesn't exist in stable Rust; all cleanup must be sync).

7. **Process groups vs. task graph** — the engine's task graph (recursive parent-child) is separate from the OS's process groups. A task spawns processes in its own group(s); when the task cancels, `ctx.stop_all()` sends signals to those groups. Make sure the two trees don't get confused in documentation or error messages.
