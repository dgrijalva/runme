# Soft Restart

## Status: complete

## Goal

Add a cooperative restart mode alongside the existing hard restart. Hard restart kills the task subtree and respawns it (current behavior). Soft restart sends a signal that the task can subscribe to and handle however it wants — reload config, drain in-flight work, re-exec a child, etc. Tasks that don't subscribe transparently fall back to hard restart.

## Context

The note that seeded this plan: `docs/open_issues.md` § "Soft vs hard restart".

Current restart surface:
- `EngineHandle::restart(id)` — hard restart only. `src/execution/engine.rs:778`.
- Wired to TUI `r`. `src/tui/event.rs:96`.
- No restart in the MCP frontend today.

## Design Decisions

All decisions below were settled in conversation. Anything still open is marked `OPEN`.

### Task-facing API

`TaskContext` gains:

```rust
fn restart_handle(&self) -> RestartHandle
```

`RestartHandle` surface:

```rust
impl RestartHandle {
    async fn wait(&mut self);              // await next signal; select!-compatible
    fn should_restart(&mut self) -> bool;  // non-async poll; consumes pending signal
}
```

Internals (`watch` or equivalent) are not part of the public surface. The handle is whatever wrapper hides them.

### Signal semantics

- **Single-slot, debounced.** Multiple soft restarts that fire before the task drains collapse into one pending signal.
- **Fire-and-forget from the runner.** The runner does not wait for the task to acknowledge or finish handling. No timeout, no escalation to hard restart.
- **Fallback to hard.** If a task never called `restart_handle()` (no receivers exist on the sender side), a soft restart request is executed as a hard restart instead.

### Scope of "the task"

Each task invocation — top-level or spawned child — has its own `TaskContext` and therefore its own restart handle. Soft restart is delivered to the specific task being restarted (top-level from TUI / MCP). Spawned children are independent; if a parent wants to propagate, that's the parent's logic.

### Engine API

```rust
enum RestartMode { Soft, Hard }

impl EngineHandle {
    async fn restart(&self, id: TaskId, mode: RestartMode) -> Result<TaskId, RestartError>;
}
```

Replaces the existing single-mode `restart(id)`.

### TUI

- `r` → soft restart (default for the most-used path)
- `R` → hard restart

### MCP

New tool `restart_task` with `mode: "soft" | "hard"` param. Default `soft`.

### CLI

In CLI mode (running a task to completion in the foreground, no TUI), `SIGHUP` on the `rnme` process triggers a soft restart of the running task. Other signals unchanged.

## Open Questions

None at plan time. Anything that surfaces during implementation should be raised before being decided.

## Implementation Slices

Slices are vertical where practical. Per project convention, intermediate slices do not need to build cleanly.

### Slice 1 — Engine + handle plumbing

- Define `RestartMode` enum in `src/execution/control.rs` (or wherever `RestartError` lives).
- Extend `Control::RestartTask` payload with `mode`.
- Update `EngineHandle::restart(id, mode)` signature and the root loop dispatch in `src/execution/root.rs`.
- Hard path keeps current behavior; soft path is a stub that always falls back to hard for now.
- Update existing tests / call sites for the new signature.

### Slice 2 — `RestartHandle` on `TaskContext`

- Add the signal channel (`watch` or equivalent) to the task's runtime state.
- Implement `RestartHandle` with `wait()` + `should_restart()`.
- Expose `ctx.restart_handle()`.
- Sender side: lives wherever the engine routes restart to a running task. Engine soft-restart path becomes: "if any receiver exists on this task's channel, fire signal; else hard restart."
- Test: task that subscribes receives a signal; task that doesn't subscribe gets hard-restarted.

### Slice 3 — TUI wiring

- TUI `r` handler: pass `RestartMode::Soft` to `EngineHandle::restart`.
- New `R` handler: pass `RestartMode::Hard`.
- Footer / help overlay text updated.

### Slice 4 — MCP tool

- Add `restart_task` tool with `mode` param, default `soft`.
- Wire to `EngineHandle::restart`.

### Slice 5 — CLI SIGHUP

- In CLI mode, install a `SIGHUP` handler that issues a soft restart on the currently running top-level task.
- Verify other signal paths are unchanged.

## Acceptance Criteria

- [x] `ctx.restart_handle()` returns a `RestartHandle` usable in `select!` and via non-async `should_restart()`.
- [x] Multiple soft-restart signals that fire before the task drains collapse into one.
- [x] Soft restart on a task with no `restart_handle()` receivers falls back to a hard restart.
- [x] TUI `r` → soft, `R` → hard.
- [x] MCP `restart_task` tool exists with `mode: "soft" | "hard"`, default soft.
- [x] CLI `SIGHUP` triggers soft restart on the running task.
- [x] Existing hard-restart behavior preserved (kill subtree + respawn).

## Implementation Notes

Decisions taken during implementation that weren't fully spelled out in the plan above:

- **`watch::Sender::new(0u64)` everywhere.** The soft-restart channel is a `tokio::sync::watch::<u64>` (counter incremented per fire). The `TaskExecution` owns the sender and clones it into the `TaskContext` in `spawn_body`. `TaskContext::new` constructs a no-op sender so `ctx.restart_handle()` works outside the engine runtime (tests using `TaskContext::new` directly) — it just never fires.
- **Subscription detection is sender-side.** The engine checks `Arc<watch::Sender<_>>::receiver_count() > 0` to decide whether a soft restart is deliverable. While at least one `RestartHandle` is held, soft is delivered; otherwise the engine falls through to hard.
- **TUI viewport reset is gated on id change.** Soft restart with a subscriber returns the same `TaskId`. The TUI now only resets follow / scroll / search / detail state when the restart produced a fresh id (hard, or soft that fell back to hard).
- **MCP supervisor learns about new tops on hard fallback.** Added `Supervisor::restart_task` (not just a direct `request_addr` passthrough) so that when a hard restart produces a fresh top-level task, the supervisor's `engine_map` gets the new top → gen mapping. Without this, subsequent MCP calls referencing the new id wouldn't route.
- **Tool response shape.** `restart_task` returns `{ "task_id": "<dotted>" }` matching the shape of `spawn_task`'s `task_id` field (dotted address, string), not the raw engine-internal id.

## Testing

- Engine unit tests in `src/execution/engine.rs`:
  - `soft_restart_delivers_signal_when_subscribed` — task subscribes, soft restart returns same id, subscriber observes a signal.
  - `soft_restart_falls_back_to_hard_without_subscriber` — task without subscription gets hard-restarted; new id returned, old task transitions to Cancelled.
  - Existing hard-restart tests updated to pass `RestartMode::Hard` explicitly.
- MCP integration tests in `tests/mcp_tools_smoke.rs`:
  - `restart_task_falls_back_to_hard_without_subscriber` — `:list` (non-subscriber) gets a new dotted address back.
  - `restart_task_rejects_unknown_mode` — bad `mode` string surfaces a parse error.
- Manual / interactive testing via the new `soft_restart_demo` task in `docs/examples/RUNME.rs` — runs an uptime counter that resets on soft restart and grows again. Try:
  - CLI: `rnme soft_restart_demo` then `kill -HUP <pid>` from another shell.
  - TUI: `rnme` then `r` (soft) or `R` (hard).
  - MCP: call `restart_task` with `mode: "soft"`.
