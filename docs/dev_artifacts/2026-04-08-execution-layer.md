# Extract Shared Execution Layer

## Context

CLI and TUI modes independently orchestrate task execution: each creates a `TaskContext`, wires up output forwarding, monitors spawn events, and handles cleanup. This means every execution concern is implemented twice with subtle differences (e.g., two separate process tracking mechanisms, different tracing strategies). Bugs like process orphaning on exit had to be fixed in both paths independently.

The goal: a shared execution layer that both UIs are thin wrappers over. The execution engine runs tasks, captures all output, tracks processes, and manages lifecycle. The UI subscribes to its output and renders it.

## Plan

### New file: `crates/runme/src/execution.rs`

The `TaskExecution` struct represents a single task execution. It owns everything from "task starts" to "task finishes and processes are cleaned up."

```rust
pub struct TaskExecution {
    task_name: String,
    log_store: Arc<Mutex<LogStore>>,
    task_status: Arc<Mutex<TaskStatus>>,
    processes: Arc<Mutex<Vec<ProcessState>>>,
    task_handle: Option<JoinHandle<()>>,
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,
    tracing_installed: Arc<AtomicBool>,  // shared across executions
    registry: Option<Arc<Registry>>,
}
```

**Public API:**
- `new()` → creates LogStore, spawn channel, tracing buffer
- `set_registry()`
- `launch(task, args, config)` → creates TaskContext, installs tracing, subscribes to all buffers → LogStore, starts spawn monitor, spawns task function
- `log_store()` → `&Arc<Mutex<LogStore>>` for UI subscription
- `task_status()` → `&Arc<Mutex<TaskStatus>>`
- `processes()` → `&Arc<Mutex<Vec<ProcessState>>>`
- `wait()` → awaits the task JoinHandle
- `shutdown(timeout)` → SIGTERM all tracked pgids, wait, SIGKILL survivors

### Types that move from `tui/runner.rs` to `execution.rs`

- `TaskStatus` (Setup / Ready / Done / Failed) — task lifecycle, not UI
- `ProcessStatus` (Running / Done / Failed / Stopped) — process lifecycle, not UI
- `ProcessState` (renamed from `ProcessInfo`) — execution-layer process tracking: pid, pgid, status, command_label, output buffer. The TUI can read these for display, but they're owned by the execution layer.

`tui/runner.rs` re-exports these so existing TUI imports keep working.

### Helpers that move to `execution.rs`

- `start_buffer_forwarder()` — subscribes an OutputBuffer to forward into LogStore
- `start_tracing_forwarder()` — subscribes a tracing broadcast receiver to forward into LogStore
- `monitor_spawns()` — receives SpawnEvents, creates ProcessState, subscribes to each buffer → LogStore

These are currently in `tui/runner.rs` but have zero TUI dependencies.

### Tracing: unified across modes

Today CLI uses `tracing_subscriber::fmt` → stderr, TUI uses `LogEntryLayer` → OutputBuffer → LogStore. With the execution layer owning LogStore, both modes use `LogEntryLayer`. The CLI subscribes to LogStore and writes entries to stdio — same as it does for exec/spawn output.

This means CLI tracing output format changes slightly (it'll render `LogEntry.raw` instead of the fmt subscriber's format), but it's now consistent with how all other output is rendered.

### `LaunchConfig` for optional hooks

```rust
pub struct LaunchConfig {
    pub tui_wait: Option<Arc<AtomicBool>>,
    pub tui_output: Option<Arc<Mutex<TuiOutput>>>,
}

impl Default for LaunchConfig {
    // None for both — CLI/Agent don't need these
}
```

CLI passes `LaunchConfig::default()`. TUI passes the hooks. The execution layer forwards them to `TaskContext` if present.

### How CLI changes

`run_cli()` becomes:

```rust
async fn run_cli(task, args, registry) {
    let mut exec = TaskExecution::new();
    exec.set_registry(registry.clone());

    // Subscribe to LogStore → stdio (before launch)
    let rx = exec.subscribe().await;
    tokio::spawn(forward_output_to_stdio(rx));

    exec.launch(task, args, LaunchConfig::default());
    exec.wait().await;
    exec.shutdown(Duration::from_secs(5)).await;

    // exit code from exec.task_status()
}
```

`run_agent()` is similar but doesn't subscribe to output.

### How TUI changes

`TaskRunner` becomes a thin TUI wrapper over `Vec<TaskExecution>`:

```rust
pub struct TaskRunner {
    executions: Vec<TaskExecution>,
    tui_wait: Arc<AtomicBool>,
    tui_output: Arc<Mutex<TuiOutput>>,
    tracing_installed: Arc<AtomicBool>,  // shared across launches
    registry: Option<Arc<Registry>>,
}
```

`TaskRunner::launch()` creates a `TaskExecution`, passes it `LaunchConfig` with the TUI hooks, calls `exec.launch()`, stores the execution.

The `TaskSession` concept either maps 1:1 to a `TaskExecution` or stays as a lightweight TUI-only wrapper for display grouping.

`TaskRunner::shutdown()` iterates all executions and calls their `shutdown()`.

`AppState` reads `execution.log_store()`, `execution.processes()`, `execution.task_status()` for rendering.

### Multi-session LogStore

When the TUI picker launches a second task, both executions need their output in the same log view. Two options:

**A.** Each `TaskExecution` has its own LogStore. `TaskRunner` merges them for the TUI (subscribe to each, push into a shared LogStore).

**B.** `TaskExecution::new()` accepts an optional shared LogStore. If provided, it pushes into that one instead of creating its own.

Option B is simpler for the TUI case and doesn't add complexity for CLI (which only has one execution). `TaskExecution::with_log_store(store)` constructor variant.

## Implementation order

1. Create `execution.rs`, move types (`TaskStatus`, `ProcessStatus`, `ProcessInfo` → `ProcessState`)
2. Move helpers (`start_buffer_forwarder`, `start_tracing_forwarder`, `monitor_spawns`)
3. Build `TaskExecution` struct with `new()`, `launch()`, `wait()`, `shutdown()`
4. Rewrite `run_cli()` to use `TaskExecution`
5. Rewrite `TaskRunner` to wrap `TaskExecution`
6. Rewrite `run_agent()` to use `TaskExecution`
7. Update tests
8. Update `docs/system_design.md` — add an Execution Layer section documenting the `TaskExecution` architecture, the boundary between execution and UI, and how output flows through LogStore. This was a critical design gap (CLI/TUI behavioral divergence, duplicated cleanup logic) that should be captured so it doesn't regress.

Each step should compile and pass tests before moving to the next.

## Critical files

| File | Change |
|---|---|
| `crates/runme/src/execution.rs` | New — shared execution layer |
| `crates/runme/src/lib.rs` | Add `pub mod execution;` |
| `crates/runme/src/tui/runner.rs` | Remove moved types/functions, wrap `TaskExecution` |
| `crates/runme/src/cli.rs` | Rewrite `run_cli` and `run_agent` |
| `crates/runme/src/tui/app.rs` | Update `launch_picked_task` |
| `crates/runme/src/tui/sidebar.rs` | Import path updates (via re-exports) |
| `crates/runme/src/tui/event.rs` | Import path updates (via re-exports) |
| `crates/runme/tests/integration.rs` | May need import updates |
| `docs/system_design.md` | Add Execution Layer section between "CLI Argument Model" and "Built-in Commands" |

The system_design.md update should cover:
- The execution/UI boundary principle: the execution layer is the system, UIs are viewports
- `TaskExecution` as the unit of task execution (owns TaskContext, LogStore, process tracking, cleanup)
- Output flow: all sources (exec, spawn, tracing) → LogStore → UI subscribes
- Process lifecycle: single tracking mechanism via execution layer, cleanup on shutdown
- How CLI/TUI/Agent modes are thin consumers of the same execution layer

## Verification

1. `cargo check` after each step
2. `cargo test` passes full suite after each step
3. Manual: `runme --ui cli <task>` shows exec + spawn output, cleans up on exit
4. Manual: `runme <task>` (TUI) shows output, process detail works, `q` cleans up
5. Manual: TUI picker launch → second task → both show output
