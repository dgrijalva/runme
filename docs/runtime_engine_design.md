# Runtime Engine Design

The engine is rnme's multi-task runtime: a recursive task graph rooted at a synthetic root, with a method-based control surface and a watch-channel snapshot of graph state. Frontends (TUI, headless CLI, agent, future MCP) consume the same engine through one shared handle.

This document is the canonical reference. For TUI-specific concerns (rendering, input, modes, sidebar shape), see `tui_design.md`. For overall system architecture, see `system_design.md`. For build pipeline, see `build_system_design.md`.

## Goal & motivation

The engine turns rnme from a single-task launcher into a multi-task runtime. Tasks can run concurrently, be spawned and terminated dynamically, and be observed (status, logs) from any number of frontends.

This unlocks:

- **TUI** — launch tasks at any time, watch them in parallel, terminate them individually, scroll back through completed ones.
- **Headless CLI** — `rnme <task>` is a degenerate consumer: spawn one child of root, await its completion, quit.
- **MCP (future)** — tools like `spawn_task`, `kill_task`, `query_logs`, `inspect_graph` map onto the same engine API.

Before the engine, multi-task plumbing lived inside the TUI (`TaskRunner`) and the rest of the system assumed a single top-level task. The engine consolidates lifecycle bookkeeping in one place; the TUI dissolved into a thin frontend that holds an `EngineHandle` and renders its state.

## Architecture

The runtime has two layers:

1. **Engine** (`src/execution/`) — multi-task task graph, control protocol, exposed state. Frontend-agnostic.
2. **Frontends** (`src/tui/`, `src/cli.rs`, future `src/mcp/`) — translate user input into engine control calls, read engine state.

```
        ┌──────────────────────────┐  ┌──────────────────────┐  ┌──────────────────┐
        │ TUI (sidebar + picker +  │  │ MCP server (future)  │  │ Headless CLI     │
        │ kill menu + log viewer)  │  │                      │  │ (`rnme <task>`)  │
        └────────────┬─────────────┘  └──────────┬───────────┘  └────────┬─────────┘
                     │ EngineHandle              │ EngineHandle          │ EngineHandle
                     ▼                           ▼                       ▼
        ┌──────────────────────────────────────────────────────────────────────────┐
        │                          Engine                                          │
        │  • Synthetic root task                                                   │
        │  • Recursive task graph (task → tasks/processes)                         │
        │  • Control channel (SpawnTask, KillTask, KillAll, Quit)                  │
        │  • Exposed state (graph snapshot, log store) — frontends read directly   │
        └──────────────────────────────────────────────────────────────────────────┘
```

### Design philosophy

The engine/frontend separation is **not** about supporting "any UI." Frontends are a known, finite set, all in this crate, all known at compile time:

- **Headless CLI** — trigger one task, propagate signals, exit when done.
- **MCP** — trigger tasks, query state, support compound operations.
- **TUI** — high-framerate observer; the most demanding consumer. Immediate-mode style: watch the graph, rebuild the rendered view on change.
- **Direct library API** — theoretical future use case (someone embedding the engine). Worth not painting ourselves into a corner over, but no specific design pass yet.

The point of separating engine from UI is to **avoid baking reusable engine capabilities into a single UI layer** — not to design infinite polymorphism upfront. The engine exposes its state straightforwardly; each UI builds whatever consumption pattern fits. New UI-specific surfaces get added when a UI needs one we haven't built — not preemptively.

## Model

### Tasks are recursive, with a synthetic root

The load-bearing model decision.

- **Tasks are recursive.** A task can spawn child tasks *and* child processes. Processes are leaves — they don't spawn tasks. The task/process boundary is structural: tasks are in-process Rust controlled via channels and async; processes are OS processes controlled via signals and syscalls.
- **A synthetic root task sits above all user tasks.** The root is a real `TaskDef`, library-provided (`src/execution/root.rs`), invisible to the user-facing task catalog. Its body is a `select!` loop over the engine's control channel. It exists to anchor the task tree, run the control loop, and provide an attachment point for runtime-level concerns.
- **Multi-task means root has N children.** The runtime holds exactly one task: the root. "Two tasks running" = "the root has two child tasks."

### Why this shape

- **Reuse over reimplementation.** All the machinery for "a node with children" — status aggregation, shutdown propagation, log composition — exists at the per-task level. The synthetic root means it applies to the multi-task case for free, instead of being reimplemented at a `Vec<TaskSession>` level on the runner.
- **A real handle for runtime-level interaction.** The root task's body and `TaskContext` become the natural attachment point for control protocol handling, runtime tracing, future cross-task coordination.
- **Composition for RUNME.rs authors.** Tasks are first-class spawnable units. `ctx.run("subtask").await` makes orchestration tasks a real composition primitive — every invocation materializes as a node in the graph.
- **Frontend-agnostic.** TUI, MCP, headless all drive the same engine.

### Unified ID space for tasks and processes

Every task and every process has a `TaskId` allocated from one global atomic counter. `LogEntry.source` is a `TaskId`. The flat engine table holds tasks (the graph keeps processes as leaves under their owning tasks); subscribers can look up either by id. Display formatting (rendering `t7` or `cargo build` with a numbered prefix) lives in the UI layer — the engine never produces display strings.

## Module layout

```
src/execution/
    mod.rs         pub use of public types
    task_id.rs     TaskId + the atomic allocator
    execution.rs   TaskExecution (recursive node), TaskStatus, TaskFailure, ProcessInfo
    builder.rs     TaskBuilder (returned by ctx.run)
    handle.rs      TaskHandle (IntoFuture + Drop)
    engine.rs      Engine, EngineHandle, EngineSpawnBuilder, GraphSnapshot, cancel ladder
    control.rs     Control enum, KillSignal, SpawnOptions, EngineError
    root.rs        Synthetic root TaskDef and its body
```

## Types

### `TaskId`

```rust
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct TaskId(pub u64);

impl TaskId {
    pub const ROOT: TaskId = TaskId(0);
    pub fn next() -> Self;  // module-static AtomicU64, starts at 1
}
```

Process-lifetime monotonic. The allocator is module-static (not engine-owned) so paths that only have a `TaskContext` can mint IDs without an engine reference. `TaskId(0)` is the synthetic root — fixed const so the root body and tests can name it directly.

### `TaskExecution` (recursive node)

```rust
pub struct TaskExecution {
    // Identity
    pub id: TaskId,
    pub task_name: String,
    pub parent: Option<TaskId>,           // None only for root

    // Graph
    pub children: Mutex<Vec<Arc<TaskExecution>>>,

    // Lifecycle
    pub cancellation: CancellationToken,  // independent, NOT a child token
    status: Arc<Mutex<TaskStatus>>,
    task_handle: Mutex<Option<JoinHandle<TaskResult>>>,
    pub abort_handle: Option<AbortHandle>,                // for the cancel-ladder abort step
    pub watchdog_abort: StdMutex<Option<AbortHandle>>,    // per-task timeout watchdog

    // Process tracking
    processes: Arc<Mutex<Vec<ProcessInfo>>>,
    spawn_tx: mpsc::UnboundedSender<SpawnEvent>,

    // Logging
    tracing_buffer: Arc<Mutex<OutputBuffer>>,
    log_store: Arc<Mutex<LogStore>>,                      // shared engine-owned reference
    pub registry: Arc<Registry>,
}
```

Wrapped as `Arc<TaskExecution>` in the engine table and on `parent.children`. The `JoinHandle` lives behind its own `Mutex<Option<...>>` so multiple consumers (handle's `.await` + the cancel ladder) can coordinate.

### `TaskStatus`

```rust
pub enum TaskStatus {
    Setup,                    // task body is executing
    Ready,                    // body signaled readiness; processes still running
    Done,                     // body returned Ok(_)
    Failed(TaskFailure),      // body returned Err(_) or panicked
    Cancelled,                // engine ran the cancel ladder against this task
    Timeout,                  // per-task watchdog fired
}
```

`Cancelled` and `Timeout` are peers of `Failed`, not subtypes. `TaskHandle.await` for a cancelled task resolves to `Err(TaskError::cancelled())`; for timeout, `Err(TaskError::timeout())`.

### `TaskHandle`

```rust
pub struct TaskHandle {
    exec: Arc<TaskExecution>,
    engine: Weak<EngineInternals>,
    armed: bool,
}

impl TaskHandle {
    pub fn id(&self) -> TaskId;
    pub fn cancellation(&self) -> CancellationToken;
}

impl IntoFuture for TaskHandle {
    type Output = TaskResult;
    // .await disarms Drop and waits for the body's JoinHandle
}

impl Drop for TaskHandle {
    // If armed, spawn a tokio task to run the single-task cancel ladder.
    // If engine is gone, fall back to signaling the token only.
}
```

The lifetime token returned by `ctx.run(name, args)`. `.await` runs the task to completion and yields `TaskResult`. **Drop without awaiting cancels this one task** (single-task ladder). Detachment is `tokio::spawn(async move { handle.await })` — the spawned tokio task owns the handle.

### `TaskBuilder`

```rust
pub struct TaskBuilder { /* ... */ }

impl TaskBuilder {
    pub fn timeout(self, d: Duration) -> Self;
    pub fn spawn(self) -> Result<TaskHandle, TaskError>;
}

impl IntoFuture for TaskBuilder {
    type Output = TaskResult;
    // .await: spawn, then await the resulting handle to completion.
}
```

Returned by `ctx.run(name, args)`. Lazy — nothing happens until `.spawn()` or `.await`. Drop without spawn is a no-op (it's a config value, not a lifetime token; lifetime semantics belong to `TaskHandle`).

### `Engine` and `EngineHandle`

```rust
pub struct Engine {
    internals: Arc<EngineInternals>,
    root_join: JoinHandle<TaskResult>,
}

#[derive(Clone)]
pub struct EngineHandle {
    pub graph: watch::Receiver<GraphSnapshot>,
    pub log_store: Arc<Mutex<LogStore>>,
    pub registry: Arc<Registry>,
    pub root: TaskId,
    // internals + control sender are private
}

impl Engine {
    pub fn start(registry: Arc<Registry>) -> (Self, EngineHandle);
    pub async fn shutdown(self);  // joins the root task
}
```

`Engine::start` spawns the synthetic root, installs the global tracing subscriber (once, gated by a `Once`), and returns the handle frontends use. `EngineHandle` is cheap to clone.

`EngineInternals` is the private state struct: the root `Arc<TaskExecution>`, the task table (`StdMutex<HashMap<TaskId, Arc<TaskExecution>>>`), the graph watch sender, the log store, the control sender, the registry. Methods on `EngineInternals` (`spawn_child`, `cancel_task_with`, `cancel_subtree_with`, `kill_all`, `timeout_task`, `kill_process`, `publish_snapshot`) are the engine's behavioral core.

### `EngineSpawnBuilder`

```rust
pub struct EngineSpawnBuilder { /* ... */ }

impl EngineSpawnBuilder {
    pub fn timeout(self, d: Duration) -> Self;
    pub async fn spawn(self) -> Result<TaskId, EngineError>;
}

impl IntoFuture for EngineSpawnBuilder {
    type Output = Result<TaskId, EngineError>;
    // .await: spawn and yield the new TaskId. Does NOT wait for completion.
}
```

Returned by `EngineHandle::spawn_task(def, args)`. **Asymmetry:** `engine.spawn_task().await` yields `Ok(TaskId)` once registered (does not wait for completion); `ctx.run().await` yields `TaskResult` after completion. This matches typical call-site needs — TUI/MCP wants the id to track; in-task composition wants the result.

### `Control` (internal protocol)

```rust
pub(crate) enum Control {
    SpawnTask  { def: &'static TaskDef, args: Vec<String>, opts: SpawnOptions, reply },
    KillTask   { id: TaskId, signal: KillSignal, reply },
    KillAll    { reply },
    Quit       { reply },
}

pub enum KillSignal {
    Term,   // cancel ladder with kill_timeout=2s
    Kill,   // cancel ladder with kill_timeout=0 — SIGKILL processes immediately
}

#[derive(Default, Clone)]
pub struct SpawnOptions {
    pub timeout: Option<Duration>,
    // Designed to grow: future fields (ready_when, env overlay, etc.)
}
```

`Control` is `pub(crate)` — frontends never construct it directly. `EngineHandle` methods serialize into Control messages with `oneshot` reply channels. The synthetic root's body owns the receiver in a `select!` loop. This keeps the public surface stable as the protocol evolves.

`Control::KillAll` is its own variant, not a `KillSignal::All`: semantics differ ("zero out the runtime, root stays alive"). `Quit` cancels the entire ROOT subtree and exits the runtime.

### `GraphSnapshot`, `TaskNode`, `ProcessNodeInfo`

```rust
#[derive(Clone)]
pub struct GraphSnapshot {
    pub root: TaskId,
    pub tasks: Arc<HashMap<TaskId, TaskNode>>,
}

#[derive(Clone)]
pub struct TaskNode {
    pub id: TaskId,
    pub name: String,
    pub parent: Option<TaskId>,
    pub children: Vec<TaskId>,
    pub status: TaskStatus,
    pub processes: Vec<ProcessNodeInfo>,
}

#[derive(Clone)]
pub struct ProcessNodeInfo {
    pub id: TaskId,
    pub task_name: String,
    pub command_label: String,
    pub pid: Option<u32>,
    pub pgid: Option<i32>,
    pub status: ProcessStatus,
    pub ready: bool,
}
```

Immutable snapshots. `Arc<HashMap>` keeps clones cheap. New snapshots are produced on every lifecycle event (spawn, status change, process appeared/exited, readiness flip, cancel ladder finished). `GraphSnapshot::source_labels()` returns a `HashMap<TaskId, String>` for UI rendering.

### `EngineError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is shutting down")]
    ShuttingDown,
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("{0}")]
    Task(#[from] TaskError),
}
```

## Control protocol

The public surface is method-based on `EngineHandle`:

```rust
impl EngineHandle {
    pub fn spawn_task(&self, def: &'static TaskDef, args: Vec<String>) -> EngineSpawnBuilder;
    pub async fn kill_task(&self, id: TaskId, signal: KillSignal) -> Result<(), EngineError>;
    pub async fn kill_all(&self) -> Result<(), EngineError>;
    pub async fn kill_process(&self, process_id: TaskId, signal: KillSignal) -> Result<(), EngineError>;
    pub async fn quit(&self) -> Result<(), EngineError>;
    pub fn subscribe_logs(&self) -> broadcast::Receiver<LogEntry>;
    pub fn lookup(&self, id: TaskId) -> Option<Arc<TaskExecution>>;
    pub fn source_ids_for(&self, task_id: TaskId) -> Vec<TaskId>;
}
```

Each method (except the synchronous `lookup`, `source_ids_for`, and `subscribe_logs`) constructs a `Control` message with a `oneshot::Sender` for the reply, sends on the unbounded mpsc channel, and awaits the reply. Receiver-dropped (root body exited unexpectedly) becomes `EngineError::ShuttingDown`.

`kill_process` is direct (calls `EngineInternals::kill_process` without going through the control loop) because a process kill mutates only the process, not the task graph — the 250ms signal-0 watcher catches the resulting exit and republishes the snapshot.

The control mpsc is **unbounded** because control messages are infrequent (user keypresses, MCP tool calls) and back-pressure on the picker would be worse than memory growth.

### Synthetic root body

The root's body (`src/execution/root.rs`) is a `select!` loop:

```rust
loop {
    tokio::select! {
        _ = root_token.cancelled() => break,    // engine.cancel() path
        msg = control_rx.recv() => match msg {
            Quit { reply }     => { reply.send(Ok(())); break }
            SpawnTask { .. }   => spawn_child(ROOT, def, args, opts) under tokio::spawn
            KillTask { .. }    => spawn cancel_subtree_with(id, kill_timeout)
            KillAll { .. }     => spawn kill_all()
        }
    }
}
// On Quit: cancel_subtree(ROOT), then return.
```

Replies fire **before** the cancel walk so callers awaiting `quit()` / `kill_task()` don't block on subtree teardown.

### The spawn primitive

`EngineInternals::spawn_child(parent_id, def, args, opts)` is the canonical entry point. Both `TaskBuilder` (from `ctx.run`) and `EngineSpawnBuilder` (from `EngineHandle::spawn_task`) funnel through it. The synthetic root's `SpawnTask` arm calls it with `parent_id = TaskId::ROOT`, so picker-launched tasks and `ctx.run`-launched tasks share the same code path.

Registration (table insert + `parent.children` push) happens **synchronously, before** the body's tokio task launches. This eliminates the race where `lookup(id)` could return `None` for a task whose body had already started running.

If `opts.timeout.is_some()`, `spawn_child` also spawns a watchdog tokio task and stores its `AbortHandle` on `exec.watchdog_abort` for the cancel ladder to abort.

## Cancellation model

Cancellation and completion are **separate signals**.

- **Completion** = body returned `Ok(_)` or `Err(_)`. Sets status to `Done` / `Failed`. Does NOT touch children. Detached children continue running independently.
- **Cancellation** = explicit user/engine action: TUI kill key, `Control::KillTask`, `TaskHandle::Drop`, or per-task timeout. Propagation to children is engine-walked, not via tokens.

### Independent tokens (no parent linkage)

Each `TaskExecution` holds its own `CancellationToken`, constructed via `CancellationToken::new()`. We deliberately do **not** use `parent.cancellation.child_token()`. The graph (`Engine::table` + `TaskExecution::children`) is the source of truth for parent-child relationships, and propagation is an explicit recursive walk.

### Two verbs: single-task vs subtree

| Trigger | Propagation | Calls |
|---|---|---|
| `TaskHandle::Drop` (un-awaited) | **Single task only.** Rust's drop chain handles the rest. | `engine.cancel_task(id)` |
| `EngineHandle::kill_task(id, Term)` | **Subtree.** Walk graph, cancel each. | `engine.cancel_subtree(id)` |
| `EngineHandle::kill_task(id, Kill)` | **Subtree** with `kill_timeout=0`. | `engine.cancel_subtree_with(id, 0)` |
| `EngineHandle::kill_all()` | **Each direct child of root.** Root stays alive. | `engine.kill_all()` |
| `EngineHandle::quit()` | **Whole graph**, then root body returns. | `engine.cancel_subtree(ROOT)` |
| Process signal handler (Ctrl-C) | **Whole graph** via `quit()`. | `engine_handle.quit().await` |
| Per-task timeout watchdog | **Single task only**, status set to `Timeout`. | `engine.timeout_task(id)` |

**Why drop is single-task, not subtree:** Rust's drop chain already handles propagation correctly. If A holds B's handle and B's body did `let c = ctx.run("C"); c.await?`, then dropping A's handle aborts B's tokio task → B's stack drops → C's handle drops → C's drop fires its own ladder. C cancels. Same setup but B did `tokio::spawn(async move { ctx.run("C").await })` — C's handle lives in a separately-spawned future, not on B's stack. Aborting B's task doesn't touch C; C lives. This is the detachment promise.

Engine-walked subtree cancel is the **explicit user-action** path: when the user clicks "kill A," they want the visible branch nuked regardless of whether children were detached.

**API surface invariant:** `TaskHandle::Drop` calls `cancel_task` only. `Control::KillTask` / `Control::Quit` call `cancel_subtree`. `Control::KillAll` calls `kill_all`. The watchdog calls `timeout_task`.

### The cancel ladder (single-task)

```
1. Abort the task's timeout watchdog if any (prevents Cancel→Timeout overwrite).
2. Cancel the token (no-op for tasks that don't observe it).
3. ctx.stop_all(kill_timeout) — SIGTERM each owned process group, escalate to SIGKILL after kill_timeout.
4. Wait CANCEL_TIMEOUT (2s) for the body's tokio task to exit.
5. Still alive? abort_handle.abort() — sync, idempotent.
6. If status was Setup/Ready, write TaskStatus::Cancelled.
```

`timeout_task` is the same ladder except step 6 writes `Timeout` instead of `Cancelled` (and it doesn't re-abort the watchdog since the watchdog is the caller).

`AbortHandle` is stored on `TaskExecution` alongside the JoinHandle (cheap clone of `JoinHandle::abort_handle()`). Step 5 uses it without re-locking.

### Cancellation is opt-in for task bodies

Most tasks won't observe `cancellation.cancelled()` directly. Their `ctx.exec(...).await?` and `srv.next().await` calls return errors when their owned processes die under them, and that's enough. The engine's ladder kills processes; bodies unwind naturally.

For tasks that want explicit early-out, `TaskContext` exposes three forms:

```rust
impl TaskContext {
    pub fn cancelled(&self) -> bool;                              // sync bool sugar
    pub fn cancellation_signal(&self) -> WaitForCancellationFuture<'_>;  // future for select!
    pub fn cancellation(&self) -> CancellationToken;              // raw token to clone/pass
}
```

Usage:

```rust
while !ctx.cancelled() { do_work().await?; }

loop {
    tokio::select! {
        _ = ctx.cancellation_signal() => break,
        ev = w.next() => { /* ... */ }
    }
}
```

### Engine-external shutdown

The canonical shutdown is `engine_handle.quit().await`. The CLI signal handler (Ctrl-C) calls this. The `Engine` owner can also call `engine.cancel()` directly, which fires the root's token; the root's `select!` exits and flows into `cancel_subtree(ROOT)`. Both paths converge.

## Process lifecycle

All processes are spawned through `TaskContext::spawn()` / `exec()` (sugar for `spawn().complete()`). Every spawn:

1. Builds a `Cmd`, spawns a `tokio::process::Child` in its own process group.
2. Starts a **per-process reaper task** that owns the `Child`, calls `wait()` on it, and publishes the exit status on a oneshot.
3. Emits a `SpawnEvent` so the engine registers the new process in the owning task's `ProcessInfo` list.
4. Runs the readiness probe if configured (`ready_on_port`, `ready_on_http`, `ready_when`); on success, sets `ready: true` on the `ProcessInfo`.
5. Forwards stdout/stderr lines through the parser pipeline into the engine's `LogStore`, tagged with the process's `TaskId`.

The reaper task ensures **zombies don't accumulate** even when task code never awaits its `ProcessHandle` — the kernel can collect the exit status as soon as the process dies because the reaper called `wait()`. The reaper publishes the exit status; `ProcessHandle::wait()` / `complete()` consume it from the oneshot.

### `ProcessHandle::Drop`

Mirrors `TaskHandle::Drop`: an `armed: bool` flag is set when the handle is constructed, cleared by `wait()` / `complete()` / `stop()`. If still armed at Drop, a sync send-SIGTERM-to-pgid runs. Drop does not wait for exit; the reaper running in the background handles cleanup. Sub-second exits are detected by the engine's signal-0 watcher.

### Signal-0 exit watcher

The engine runs a background loop polling `killpg(pgid, 0)` every 250ms (`monitor_spawns`) for each process in `Running` status. When the probe fails (process gone), the watcher consults the reaper's published exit status, transitions the `ProcessStatus` to `Done(0)` / `Failed(code)` / `Stopped`, and publishes a fresh graph snapshot. This is also what `EngineHandle::kill_process` relies on to surface kill results.

### `ProcessStatus`

```rust
pub enum ProcessStatus {
    Running,
    Done,            // exit code 0
    Failed(i32),     // non-zero exit, signal, or timeout
    Stopped,         // user-initiated stop via the TUI
}
```

## Timeouts

Per-invocation, configured at the call site — there is **no `TaskDef::timeout` field**. Different callers may want different timeouts for the same task definition.

Three call sites:

| Site | Form |
|---|---|
| In task code | `ctx.run("foo", &[]).timeout(d).await?` |
| Frontend via engine handle | `engine_handle.spawn_task(def, args).timeout(d).await?` |
| CLI flag | `rnme --timeout 30s build` — parsed in `src/cli.rs` and applied to the builder |

The setting flows through `SpawnOptions::timeout: Option<Duration>`. `EngineInternals::spawn_child` consumes `SpawnOptions`; both builders populate it from their `.timeout()` setter.

### Watchdog wiring

If `opts.timeout.is_some()`, `spawn_child` spawns a watchdog tokio task:

```rust
let watchdog = tokio::spawn(async move {
    tokio::time::sleep(d).await;
    engine.timeout_task(id).await;
});
*exec.watchdog_abort.lock().expect(...) = Some(watchdog.abort_handle());
```

`cancel_task_with` aborts the watchdog as its first step — without this, a Cancel→Timeout race would let the delayed watchdog overwrite `Cancelled` with `Timeout`.

### Why timeouts stress-test cancellation

The watchdog exercises the same ladder as Drop, KillTask, KillAll, and Quit — if any of them behaves differently, the abstraction is wrong. Implementing timeouts caught the watchdog-abort step as a real concern; it was added before any user-facing timeout shipped.

## Logging

A single engine-owned `LogStore` collects entries from every task and every process. Source keys are integer `TaskId` values in a unified namespace shared by tasks and processes. Display formatting (rendering `t7` or `cargo build [N]`) lives in the UI layer.

### Pipeline

- **Subprocess output:** `monitor_spawns` reads stdout/stderr lines, runs them through the parser chain (`RecordParser`: JSON, logfmt, cargo diagnostics, rust panic, plain text), produces `LogEntry`s tagged with the process's `TaskId`, pushes into the per-process `OutputBuffer` and into the engine's `LogStore`.
- **Task tracing:** A single global `tracing::Subscriber` is installed once when `Engine::start` runs. It uses a task-local `TASK_TRACING_CTX` set by `spawn_body` for each task body, so events route to the right buffer with the right `TaskId`. `info!`/`error!`/etc. interleave with subprocess output in the log viewer.
- **Raw text:** `ctx.println(line)` pushes a raw `LogEntry` into the task's buffer. Used by built-ins that need plain text (`rnme list`).

### Subscription and filtering

```rust
impl EngineHandle {
    pub fn subscribe_logs(&self) -> broadcast::Receiver<LogEntry>;
    pub fn source_ids_for(&self, task_id: TaskId) -> Vec<TaskId>;
}
```

`subscribe_logs` returns the `LogStore`'s broadcast receiver. `source_ids_for` walks the graph snapshot from `task_id` and returns every descendant source id (tasks AND processes). Frontends filter incoming entries by membership in this set; `LogStore::output_for_many` is the convenience builder.

Tasks and processes share the ID namespace, so duplicate-name disambiguation (two `cargo build` tasks) is automatic — their IDs differ. The TUI uses source colors first, numbered `[N]` prefixes as a fallback when colors run out.

## Graph snapshot & observation

```rust
pub graph: watch::Receiver<GraphSnapshot>
```

The engine pushes a fresh `GraphSnapshot` on every lifecycle event:

- New task spawned (registered)
- Task status changed (Setup→Ready, →Done, →Failed, →Cancelled, →Timeout)
- Process appeared (`SpawnEvent`)
- Process readiness flipped
- Process status changed (signal-0 watcher detected exit)
- Cancel ladder finished

Snapshots are immutable. `Arc<HashMap>` makes clone cheap. `watch::changed().await` integrates cleanly with frontend event loops.

Why `watch::Receiver<GraphSnapshot>` and not `Arc<RwLock<Graph>>`:

1. The TUI's preferred consumption is "watch and rebuild on change" — `watch` is exactly that.
2. `watch::changed()` integrates with `tokio::select!`.
3. Snapshots are immutable values; no reader/writer lock contention.

Recomputation is O(graph size); fine for human-scale task counts.

### Completed tasks stay

The engine never garbage-collects nodes. Their logs and lifecycle history remain available. Memory pressure is a future problem worth tolerating for the navigability win — being able to scroll back into a finished task's output is too valuable to drop.

## Frontends

### TUI

The primary frontend (`src/tui/`). Holds an `EngineHandle`, watches the graph for changes, subscribes to the log store, and emits control calls when the user acts. Picker selection emits `EngineHandle::spawn_task`; the kill submenu emits `kill_task` / `kill_all`; quit confirmation emits `quit`. See `tui_design.md`.

### Headless CLI

`rnme <task>` (`src/cli.rs::run_cli`). Routes through the engine like every other frontend:

1. `Engine::start(registry)`.
2. Subscribe to `LogStore` and forward entries to stdio (with color and format respected).
3. `engine_handle.spawn_task(def, args).timeout(d?).await` to spawn the task as a child of the synthetic root.
4. Watch the graph snapshot for terminal status on the spawned id.
5. Two-phase wait: first the body reaches a terminal status, then keep waiting if any owned processes are still running.
6. Ctrl-C calls `engine_handle.quit().await`; clean teardown.

Exit codes: `0` Done, task's exit code on Failed, `124` Timeout, `130` Cancelled / Ctrl-C.

### Agent mode

`run_agent` in `src/cli.rs` is the same engine path as headless, but skips stdio forwarding — only the final result is reported (JSON or text). Quiet by default, structured output for programmatic consumers. Currently has no CLI entry point: the previous `--ui agent` flag was removed when the UI-mode flags became bare (`--tui`, `--cli`, future `--mcp`); this code path will become reachable again when `--mcp` lands and either replaces or wraps it.

### MCP (future)

Not built. The engine's existing surface (`spawn_task`, `kill_task`, `subscribe_logs`, `graph` snapshot) is the substrate it will sit on. Tool calls (`spawn_task`, `kill_task`, `query_logs`, `inspect_graph`, `quit`) translate into engine calls. MCP-specific concerns (tool schemas, authentication, streaming responses, edit-and-rebuild) live in a future design pass — see `open_issues.md` for the rebuild-on-edit problem.

## Risks & future work

- **Resource use grows linearly with task count.** No backstop today against spawning 50 long-running tasks. Probably fine for a single user; revisit if it bites.
- **Sidebar density.** Two tasks each spawning 3 processes is already 8 entries before completed tasks. The TUI sidebar redesign needs to handle density (collapsing, grouping, filtering) without becoming a separate project. Tracked in `open_issues.md`.
- **Engine API stability.** Once MCP starts depending on the engine's public API, breaking changes get more expensive. Pre-release status mitigates this for now, but the engine API is the surface we'll most regret churning later. The method-based `EngineHandle` with private `Control` was a deliberate choice for this reason.
- **Engine-as-child-process for MCP.** MCP servers live for the agent session, but rnme builds the user's RUNME.rs into a binary at startup. Edit-and-rebuild during a session means the in-process engine is stale. Likely answer: MCP supervisor mode where the supervisor spawns the real engine as a child and rebuilds it on file change. Requires an IPC protocol; deferred until MCP design starts.
- **Per-task TUI quit preference.** Tasks could declare a preferred behavior for "what happens to the TUI when I finish" (Quit / Stay / NoOpinion). Probably subsumed by the existing CLI-vs-TUI default mode (`UiHint`); revisit if the multi-task TUI use surfaces a real need.
- **Source identity strings.** Today log source keys are `TaskId`. If MCP or other consumers need a stable string representation across engine restarts, this is the place to extend.
