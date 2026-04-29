# Architecture: Multi-Task Runtime — Internal Type Definitions

Synthesis of `research-execution.md`, `research-cancellation.md`, and `research-task-context.md` against `docs/03-multi-task-runtime.md`. This is the engine implementor's spec. Every decision below is final unless flagged in "Open questions for human review".

## Cancellation model (load-bearing)

Cancellation and completion are **separate signals**. Read this section before §2/§3/§6/§7/§8 — it changes how those types are wired.

- **Completion** = task body returned `Ok(_)` or `Err(_)`. Updates the task's status to `Done` / `Failed`. Does NOT touch children. Children that were detached via `tokio::spawn(async move { ctx.run(...).await })` continue running independently. (Settled: option A. If a parent wants children to die when it errs, it must explicitly drop or cancel their handles.)
- **Cancellation** = explicit user/engine action: TUI kill key, `Control::KillTask`, or `TaskHandle::Drop` for an un-awaited handle. Propagation to children is done by the engine **walking the graph** and cancelling each token explicitly — *not* via `CancellationToken::child_token()`.

### Tokens are independent (no parent linkage)

Each `TaskExecution` holds its own `CancellationToken`, constructed via `CancellationToken::new()`. We deliberately do **not** use `parent.cancellation.child_token()`. The parent→children relationship lives in the engine's graph (`children: Arc<Mutex<Vec<Arc<TaskExecution>>>>`), and propagation is an explicit recursive walk:

```rust
fn cancel_subtree(engine: &EngineInternals, root_id: TaskId) {
    let Some(exec) = engine.lookup(root_id) else { return };
    engine.cancel_task(exec);  // see fallback ladder below
    let children = exec.children.blocking_lock().clone();
    for child in children {
        cancel_subtree(engine, child.id);
    }
}
```

(Use async lock in the actual implementation; the snippet uses `blocking_lock` for clarity.)

### Cancellation is opt-in for task bodies

Most tasks won't observe `cancellation.cancelled()` directly. Their `ctx.exec(...).await?` and `srv.next().await` calls will return errors when their owned processes die, and that's enough. The engine handles cancellation via a **fallback ladder**:

1. Cancel the task's `CancellationToken` (signals any task body that opted into observing it).
2. Call `ctx.stop_all(Duration::from_secs(2))` on the task — terminates its owned process groups via the existing SIGTERM → 2s → SIGKILL escalation (see `src/process.rs:900`).
3. Wait a fixed **2s** for the task body's tokio task to exit on its own. Most do, because their `ctx.exec(...).await?` paths bubble `Err` once their child processes die.
4. If still alive after step 3: `task_handle.abort()` on the tokio JoinHandle.
5. Set status to `TaskStatus::Cancelled`.

The 2s constants match the existing process-level escalation; engine-wide for now.

### `TaskStatus::Cancelled` and `Timeout` are real variants

Both are siblings of `Failed`, not subtypes. The full `TaskStatus` enum:

```rust
pub enum TaskStatus {
    Setup,
    Ready,
    Done,
    Failed(TaskFailure),
    Cancelled,
    Timeout,
}
```

`TaskHandle.await` for a cancelled task resolves to `Err(TaskError::cancelled())`; for a timed-out task, `Err(TaskError::timeout())`. UI rendering of both follows the same pattern as `Failed` / `Done` (added in `src/tui/sidebar.rs::task_status_display`). `Timeout` lands in slice 2 alongside `Cancelled` so per-task timeouts (§11) work in slice 4 without a status-enum followup.

### Two verbs: single-task vs subtree

Cancellation has **two distinct propagation rules**, depending on who triggered it:

| Trigger | Propagation | Calls |
|---|---|---|
| `TaskHandle::Drop` (un-awaited) | **Single task only.** Rust's drop chain handles the rest. | `engine.cancel_task(exec.id)` |
| `EngineHandle::kill_task(id, Term)` | **Subtree.** Walk graph, cancel each. | `engine.cancel_subtree(id)` |
| `EngineHandle::kill_task(id, Kill)` | **Subtree.** Same as Term but with `kill_timeout=0`. | `engine.cancel_subtree_with(id, 0)` |
| `EngineHandle::kill_all()` | **Each direct child of root.** Root stays alive. | `engine.kill_all()` |
| `EngineHandle::quit()` | **Whole graph**, then root body returns. | `engine.cancel_subtree(TaskId::ROOT)` |
| Process signal handler (Ctrl-C in headless) | **Whole graph** via `quit()`. | `engine_handle.quit().await` |
| Per-task timeout watchdog | **Single task only**, status set to `Timeout`. | `engine.timeout_task(id)` |

**Why drop is single-task, not subtree:** Rust's drop chain already does the propagation correctly:

- A holds B's handle. B's body did `let c = ctx.run("C"); c.await?` — C's handle lives on B's stack.
  - A drops B's handle → cancel ladder runs on B → step 4 aborts B's tokio task → B's stack drops → C's handle drops → C's drop fires its own ladder. **C cancels.**
- Same setup, but B did `tokio::spawn(async move { ctx.run("C").await })` — C's handle lives in a separately-spawned future, not on B's stack.
  - A drops B's handle → B's tokio task aborts → B's stack drops → the spawned future is **untouched** because it owns its own state. **C lives.** This is exactly the detachment promise from the design doc.

Engine-walked subtree cancel is the *explicit user-action* path: when the user clicks "kill A," they want the visible branch nuked regardless of whether children were detached. The developer-API path (`Drop`) respects ownership.

**Implementor rule:** in `TaskHandle::Drop`, never call `engine.cancel_subtree`. Only call `engine.cancel_task` (or fall back to `exec.cancellation.cancel()` if the engine is unreachable). Subtree propagation is the engine's job, triggered by `EngineHandle` methods (which serialize as `Control` messages internally) — not by Rust drops.

## 0. Module layout

The engine is consolidated under `src/execution/` (turning the existing `src/execution.rs` file into a directory module). New decision: introducing a folder is cheaper than spreading types across `task.rs` and a single `execution.rs` file.

```
src/execution/
    mod.rs         pub use of TaskExecution, TaskHandle, TaskId, Engine, Control, KillSignal
    task_id.rs     TaskId + the atomic allocator
    execution.rs   TaskExecution (recursive node)
    handle.rs      TaskHandle (IntoFuture + Drop)
    engine.rs      Engine + EngineHandle + start()
    control.rs     Control enum + KillSignal
    root.rs        Synthetic root TaskDef and its body
```

The synthetic root `TaskDef` lives in `src/execution/root.rs` rather than `src/builtin.rs` (the plan's draft put it in `builtin.rs`, but `builtin.rs` is the user-visible builtin task crate; the root must be invisible to the catalog). The plan should be updated.

## 1. `TaskId`

```rust
// src/execution/task_id.rs
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct TaskId(pub u64);

impl TaskId {
    /// The synthetic root task always has id 0. Allocated children start at 1.
    pub const ROOT: TaskId = TaskId(0);
}

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

impl TaskId {
    /// Allocate the next unique TaskId. Process-lifetime monotonic.
    pub fn next() -> Self {
        TaskId(NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "t{}", self.0)
    }
}
```

**Allocator location:** module-level `AtomicU64`. Process-global is correct: collisions across threads are impossible (atomic), and IDs never need to be reused (u64 won't wrap in any realistic process lifetime). Keeping the counter in the type's module rather than inside `Engine` means `TaskId::next()` works in tests and in code paths that don't have an `Engine` reference (e.g. the seam in `Registry::run_with_registry` only sees a `TaskContext`).

**Uniqueness scope:** process lifetime. Across engine restarts within the same process IDs continue ascending — fine because `Engine::start()` is called at most once per process today.

**`TaskId::ROOT == TaskId(0)`** is a const so the root's body and tests can name it directly.

## 2. Recursive `TaskExecution`

```rust
// src/execution/execution.rs
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

pub struct TaskExecution {
    // ── Identity ──────────────────────────────────────────────
    pub id: TaskId,
    pub task_name: String,
    /// Parent's TaskId. None only for the synthetic root.
    pub parent: Option<TaskId>,

    // ── Graph ─────────────────────────────────────────────────
    /// Direct children of this execution. Children are kept after they
    /// finish (design decision 3); the engine prunes only on Engine drop.
    pub children: Arc<Mutex<Vec<Arc<TaskExecution>>>>,

    // ── Lifecycle ─────────────────────────────────────────────
    /// Cooperative cancellation signal. **Independent — NOT a child token
    /// of the parent's.** Constructed via `CancellationToken::new()`. The
    /// engine propagates cancellation by walking `children` explicitly
    /// (see "Cancellation model" at top of document).
    pub cancellation: CancellationToken,
    /// Status: Setup → Ready → (Done | Failed | Cancelled). Shared with the
    /// running TaskContext so user code's `bind_ready` / `mark_ready` updates
    /// here. The engine writes `Cancelled` after running the cancel ladder.
    pub status: Arc<Mutex<TaskStatus>>,
    /// JoinHandle of the task body's tokio task. Always Some after launch;
    /// taken (consumed) only when awaited via TaskHandle.
    pub task_handle: Mutex<Option<tokio::task::JoinHandle<TaskResult>>>,

    // ── Process tracking (existing) ───────────────────────────
    pub processes: Arc<Mutex<Vec<ProcessInfo>>>,
    pub spawn_tx: mpsc::UnboundedSender<SpawnEvent>,

    // ── Logging ───────────────────────────────────────────────
    /// Tracing buffer for `info!`/`error!` macros from the task body.
    pub tracing_buffer: Arc<Mutex<OutputBuffer>>,
    /// Log store reference. **Engine-owned, single instance** (see §9).
    /// Each task pushes into it under a source key derived from
    /// `format!("{}#{}", task_name, id.0)`.
    pub log_store: Arc<Mutex<LogStore>>,

    // ── Registry ──────────────────────────────────────────────
    pub registry: Arc<Registry>,
}
```

**Wrap as `Arc<TaskExecution>`** in the engine's task table and in the `parent.children` list. The handle (§3) holds an `Arc` reference too; the JoinHandle is the only mutable lifecycle bit and lives behind its own `Mutex<Option<...>>` so multiple consumers (handle's await + engine's KillTask cleanup) can coordinate.

**Field migration / removal:**

| Field today | Outcome |
|---|---|
| `tui_wait`, `tui_output` (via `LaunchConfig`) | **Removed.** Plumbing on `TaskContext`, `LaunchConfig`, and `TaskExecution::launch` deleted (settled by design decision 7). |
| `log_store: Arc<Mutex<LogStore>>` per execution | **Replaced** with shared engine-owned reference. Constructor `with_log_store` becomes the only constructor; `new()` (which made a fresh store) goes away. |
| `tracing_installed: Arc<AtomicBool>` | **Hoisted to Engine.** Subscriber installation happens once when `Engine::start()` runs; `TaskExecution::launch` no longer touches it. |
| `task_handle: Option<JoinHandle<()>>` | Becomes `Mutex<Option<JoinHandle<TaskResult>>>` so the handle can move the result through `.await`. The engine retains an Arc-clone path to it for the abort step of the cancel ladder. |
| `parent`, `children`, `id`, `cancellation` | **New** — added as shown above. `cancellation` is independent (`CancellationToken::new()`), not a child token. |
| `abort_handle: AbortHandle` | **New** — clone of `task_handle.abort_handle()`. Used by the cancel ladder's step 4 without re-locking the JoinHandle slot. (§7) |
| `watchdog_abort: Mutex<Option<AbortHandle>>` | **New** — abort handle for the per-task timeout watchdog tokio task. `None` when `SpawnOptions::timeout` is `None`. Aborted by `cancel_task_with` to prevent Cancel→Timeout races. (§11) |
| `TaskStatus::Cancelled` variant | **New** — sibling of `Failed`/`Done`. Set by the engine after the cancel ladder finishes. |
| `TaskStatus::Timeout` variant | **New** — sibling of `Cancelled`. Set by `EngineInternals::timeout_task` when a task's watchdog fires. (§11) |

## 3. `TaskHandle`

```rust
// src/execution/handle.rs
use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Lifetime token returned by `ctx.run(name, args)`.
///
/// - Awaiting it yields the task's `TaskResult`.
/// - Dropping it without awaiting cancels **this one task** (single-task
///   ladder). Children are NOT walked here — Rust's drop chain handles
///   propagation: tokio task aborts → stack drops → on-stack child
///   handles drop → their drop ladders fire. Detached children
///   (`tokio::spawn(async move { ctx.run("C").await })`) are unaffected,
///   matching the design doc's detachment promise.
/// - Detachment is `tokio::spawn(async move { handle.await })`.
pub struct TaskHandle {
    /// The execution this handle is observing. Held via Arc; the engine
    /// keeps its own Arc in the task table, so dropping the handle does
    /// NOT remove the node from the graph — it only cancels.
    exec: Arc<TaskExecution>,
    /// Weak ref to the engine. Needed in Drop to invoke the async
    /// single-task ladder (which writes `TaskStatus::Cancelled` and
    /// performs the abort step). Without it, Drop falls back to a
    /// signal-only `exec.cancellation.cancel()`.
    engine: Weak<EngineInternals>,
    /// Set to false when `.await` is invoked, suppressing the Drop cancel.
    armed: bool,
}

impl TaskHandle {
    pub(crate) fn new(exec: Arc<TaskExecution>, engine: Weak<EngineInternals>) -> Self {
        Self { exec, engine, armed: true }
    }

    pub fn id(&self) -> TaskId { self.exec.id }
    pub fn cancellation(&self) -> CancellationToken { self.exec.cancellation.clone() }
}

impl IntoFuture for TaskHandle {
    type Output = TaskResult;
    type IntoFuture = Pin<Box<dyn Future<Output = TaskResult> + Send>>;

    fn into_future(mut self) -> Self::IntoFuture {
        self.armed = false; // disarm Drop — the future will own the wait
        let exec = self.exec.clone();
        Box::pin(async move {
            let join = {
                let mut slot = exec.task_handle.lock().await;
                slot.take()
            };
            match join {
                Some(h) => h.await.unwrap_or_else(|e| {
                    Err(TaskError::from_display(format!("task panicked: {e}")))
                }),
                // Already awaited — recover terminal status from the node.
                None => match &*exec.status.lock().await {
                    TaskStatus::Done => Ok(()),
                    TaskStatus::Failed(f) => Err(TaskError::from_failure(f.clone())),
                    TaskStatus::Cancelled => Err(TaskError::cancelled()),
                    _ => Err(TaskError::from_display("task handle already consumed")),
                },
            }
        })
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if !self.armed { return; }
        // Cancel THIS ONE task. Children are not walked here — Rust's
        // drop chain handles propagation through on-stack handles, and
        // detached children (in tokio::spawn) are intentionally spared.
        let id = self.exec.id;
        if let Some(engine) = self.engine.upgrade() {
            tokio::spawn(async move {
                engine.cancel_task(id).await;  // single-task ladder
            });
        } else {
            // Engine gone (runtime shutting down) — fall back to a
            // synchronous token-cancel signal. Status update and abort
            // would need the engine; skip them.
            self.exec.cancellation.cancel();
        }
    }
}
```

**Output of `.await`: `TaskResult`** (i.e. `Result<(), TaskError>`), matching the existing return type from `task.func.call`. A future evolution could expand to a richer `TaskOutcome` struct, but not in this pass.

**Relation to `TaskExecution`:** `Arc<TaskExecution>` — the handle is one observer of a node owned by the engine's task table. Drop cancels this one node; it does not remove nodes from the graph (design decision 3: completed tasks stay) and does not walk children (see "Two verbs" in the Cancellation model).

**Drop runs an async cancel via `tokio::spawn`:** the cancel ladder includes `ctx.stop_all().await` and a 2s wait, neither of which can run synchronously in `Drop`. Spawning is the only viable path — and is correct, because Drop callers don't expect a synchronous wait for cancellation to complete.

## 4. `Control` enum (internal protocol)

`Control` is the **internal** message type on the engine's command channel. It is **not** the public surface — frontends use the method-based `EngineHandle` API in §5. The methods serialize into `Control` messages internally, including `oneshot` reply channels for results.

```rust
// src/execution/control.rs
pub(crate) enum Control {
    SpawnTask {
        /// Static TaskDef from the registry. Looked up by the caller
        /// (EngineSpawnBuilder) before sending so the engine doesn't
        /// need a name-resolution dance on the control channel.
        def: &'static TaskDef,
        args: Vec<String>,
        /// Per-invocation spawn options (timeout, future fields).
        /// Constructed by the builder before sending.
        opts: SpawnOptions,
        /// Reply channel for the spawned task's id.
        reply: tokio::sync::oneshot::Sender<Result<TaskId, EngineError>>,
    },
    KillTask {
        id: TaskId,
        signal: KillSignal,
        reply: tokio::sync::oneshot::Sender<Result<(), EngineError>>,
    },
    /// Cancel each direct child of root. Root itself stays alive — this
    /// is "back to zero state," not "shut down the runtime." Sent by
    /// EngineHandle::kill_all().
    KillAll {
        reply: tokio::sync::oneshot::Sender<Result<(), EngineError>>,
    },
    Quit {
        reply: tokio::sync::oneshot::Sender<Result<(), EngineError>>,
    },
}

pub enum KillSignal {
    /// Run the cancel ladder with kill_timeout=2s.
    Term,
    /// Run the cancel ladder with kill_timeout=0 — processes get SIGKILL
    /// immediately rather than SIGTERM-then-2s-then-SIGKILL.
    Kill,
}

/// Per-invocation spawn options. Set by the builders (TaskBuilder,
/// EngineSpawnBuilder); consumed by EngineInternals::spawn_child.
///
/// Designed to grow: future fields (ready_when, env overlay, etc.) are
/// added here without touching call sites that only set what they need.
#[derive(Default, Clone)]
pub struct SpawnOptions {
    pub timeout: Option<Duration>,
}
```

`Control` is `pub(crate)`. The variants and `reply` plumbing are implementation detail behind `EngineHandle`.

**Channel ownership:**

- `tokio::sync::mpsc::UnboundedSender<Control>` — held inside `EngineHandle`. Method calls construct messages with their oneshot replies and `.send()` here.
- `tokio::sync::mpsc::UnboundedReceiver<Control>` — held inside the synthetic root task's body.

Unbounded because control messages are infrequent (user keypresses, MCP tool calls) and back-pressure on the picker would be worse than memory growth.

Receiver-dropped (root body exited unexpectedly) is treated as a closed channel by `EngineHandle` methods, which return `Err(EngineError::ShuttingDown)` from each call.

## 5. `Engine` type

```rust
// src/execution/engine.rs
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;

/// The engine. Owns the synthetic root's TaskExecution, the task graph
/// (everything under root), the LogStore, and the control channel.
pub struct Engine {
    root: Arc<TaskExecution>,
    /// Flat lookup: TaskId -> TaskExecution. Populated when ctx.run()
    /// creates a new node; never garbage-collected (design decision 3).
    table: Arc<Mutex<HashMap<TaskId, Arc<TaskExecution>>>>,
    /// Watched snapshot of the graph for cheap change-detection by UIs.
    graph_tx: watch::Sender<GraphSnapshot>,
    log_store: Arc<Mutex<LogStore>>,
    control_tx: mpsc::UnboundedSender<Control>,
    /// JoinHandle of the root task's tokio task.
    root_join: tokio::task::JoinHandle<()>,
}

/// Public handle returned to frontends. Cheap to clone.
///
/// All control surface is method-based. The internal `Control` channel
/// is hidden — methods serialize into Control messages with oneshot
/// replies. This keeps the public surface stable as the internal
/// protocol evolves.
#[derive(Clone)]
pub struct EngineHandle {
    /// Internal — do not use directly. Public methods serialize here.
    pub(crate) control: mpsc::UnboundedSender<Control>,
    /// Graph snapshot reader. Cheap to clone (broadcast-style).
    pub graph: watch::Receiver<GraphSnapshot>,
    /// Shared log store. Frontends subscribe via `subscribe_logs()` or
    /// pull snapshots through the existing LogStore API.
    pub log_store: Arc<Mutex<LogStore>>,
    /// The application's task registry, useful for picker lookup.
    pub registry: Arc<Registry>,
    /// Root TaskId, exposed so the TUI can render the "All tasks" entry
    /// as a focus on the synthetic root.
    pub root: TaskId,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine is shutting down")]
    ShuttingDown,
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("{0}")]
    Task(#[from] TaskError),
}

impl EngineHandle {
    /// Configure a spawn. Returns a builder; `.timeout(d)`, `.spawn()`,
    /// or `.await` to fire it. Default await semantics: the future
    /// resolves to `Ok(TaskId)` once the task is registered and its body
    /// is spawned — does NOT wait for completion. (Asymmetric vs
    /// `ctx.run().await` which awaits completion. See §8.)
    pub fn spawn_task(
        &self,
        def: &'static TaskDef,
        args: Vec<String>,
    ) -> EngineSpawnBuilder {
        EngineSpawnBuilder {
            handle: self.clone(),
            def,
            args,
            timeout: None,
        }
    }

    /// Cancel one task and its subtree (engine-walked, see §7).
    pub async fn kill_task(&self, id: TaskId, signal: KillSignal) -> Result<(), EngineError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.control.send(Control::KillTask { id, signal, reply: tx })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }

    /// Cancel every direct child of root. Root itself stays alive —
    /// "back to zero state," not "shut down the runtime."
    pub async fn kill_all(&self) -> Result<(), EngineError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.control.send(Control::KillAll { reply: tx })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }

    /// Shut down the runtime: cancel root subtree, then the root body
    /// returns. Awaiting `Engine::shutdown()` after this gives a clean
    /// teardown.
    pub async fn quit(&self) -> Result<(), EngineError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.control.send(Control::Quit { reply: tx })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }

    pub fn subscribe_logs(&self) -> tokio::sync::broadcast::Receiver<LogEntry> {
        // LogStore::subscribe returns the broadcast Receiver.
        // Locking briefly is fine — subscribe doesn't hold the lock.
        let store = self.log_store.try_lock().expect("log_store contention");
        store.subscribe()
    }
}

/// Builder returned by `EngineHandle::spawn_task`.
///
/// Mirrors the shape of `SpawnBuilder` for processes — same builder
/// pattern, same `.timeout()` API. Drop without spawn = no-op (nothing
/// was launched).
pub struct EngineSpawnBuilder {
    handle: EngineHandle,
    def: &'static TaskDef,
    args: Vec<String>,
    timeout: Option<Duration>,
}

impl EngineSpawnBuilder {
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Spawn the task. Resolves to `Ok(TaskId)` once registered.
    pub async fn spawn(self) -> Result<TaskId, EngineError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let opts = SpawnOptions { timeout: self.timeout };
        self.handle.control
            .send(Control::SpawnTask { def: self.def, args: self.args, opts, reply: tx })
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)?
    }
}

impl IntoFuture for EngineSpawnBuilder {
    type Output = Result<TaskId, EngineError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    /// Default await: spawn and return the new TaskId. Does NOT wait
    /// for the task to complete. Use the graph snapshot or a status
    /// subscription to track lifecycle.
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.spawn())
    }
}

impl Engine {
    /// Start the engine. Spawns the synthetic root task, returns the
    /// handle frontends use to send control and read state.
    ///
    /// `registry` is the application's task registry (today: built from
    /// inventory + InitContext). The root carries it through TaskContext.
    pub fn start(registry: Arc<Registry>) -> (Self, EngineHandle) {
        // ... see §6 for body sketch ...
    }

    pub fn handle(&self) -> EngineHandle { /* clone of internal handle */ }

    /// Wait for the root task to finish (Quit received or
    /// control channel dropped). Returns when the runtime is fully
    /// shut down.
    pub async fn shutdown(self) { /* root_join.await + final cleanup */ }
}
```

**Graph reader: `tokio::sync::watch::Receiver<GraphSnapshot>`** — chosen over `Arc<RwLock<Graph>>` because:

1. The TUI's preferred consumption pattern (per design doc) is "watch and rebuild on change" — `watch` is exactly that.
2. `watch::changed()` integrates cleanly with the TUI's existing event loop.
3. Snapshots are immutable values, eliminating reader/writer lock contention.

The `GraphSnapshot` is recomputed and pushed to the watch channel whenever a task is added, status-changes, or completes. Recomputation is O(graph size); fine for human-scale task counts.

```rust
#[derive(Clone)]
pub struct GraphSnapshot {
    pub root: TaskId,
    /// All tasks indexed by id, including completed ones.
    pub tasks: Arc<HashMap<TaskId, TaskNode>>,
}

#[derive(Clone)]
pub struct TaskNode {
    pub id: TaskId,
    pub name: String,
    pub parent: Option<TaskId>,
    pub children: Vec<TaskId>,
    pub status: TaskStatus,
    /// Each ProcessInfo carries an `id: TaskId` allocated from the same
    /// `TaskId::next()` counter — tasks and processes share an ID space
    /// (see §9). The id is what LogStore source keys reference.
    pub processes: Vec<ProcessInfo>,
}
```

`Arc<HashMap>` inside the snapshot lets clones be cheap (a single ptr-bump). The snapshot is immutable; updates produce new snapshots.

## 6. Synthetic root task body — pseudocode

```rust
// src/execution/root.rs
async fn root_body(ctx: &TaskContext) -> TaskResult {
    // The engine wires these into TaskContext before invoking root_body:
    //   ctx.engine_internals() -> Arc<EngineInternals>
    //     .control_rx: Mutex<Option<mpsc::UnboundedReceiver<Control>>>
    //     .table:      Arc<Mutex<HashMap<TaskId, Arc<TaskExecution>>>>
    //     .graph_tx:   watch::Sender<GraphSnapshot>
    //
    // The root's OWN cancellation token (ctx.cancellation()) fires when
    // an engine-external owner calls engine.cancel() (e.g., binary signal
    // handler before the control channel is wired). That's the "external
    // shutdown" arm of the select.
    let internals = ctx.engine_internals();
    let mut control_rx = internals.control_rx.lock().await.take().unwrap();
    let root_token = ctx.cancellation();
    let root_id = TaskId::ROOT;

    loop {
        tokio::select! {
            // External shutdown — engine.cancel() fired root's token directly.
            _ = root_token.cancelled() => break,

            msg = control_rx.recv() => {
                let Some(msg) = msg else { break };  // channel closed
                match msg {
                    Control::Quit { reply } => {
                        // Reply BEFORE the cancel walk so callers awaiting
                        // quit() don't block on subtree teardown.
                        let _ = reply.send(Ok(()));
                        break;
                    }

                    Control::SpawnTask { def, args, opts, reply } => {
                        // Engine owns the spawn primitive (§8). The root
                        // simply asks the engine to register a child of
                        // ROOT with the given options and replies the id.
                        let engine = internals.clone();
                        tokio::spawn(async move {
                            match engine.spawn_child(TaskId::ROOT, def, args, opts) {
                                Ok(handle) => {
                                    let id = handle.id();
                                    let _ = reply.send(Ok(id));
                                    // Awaiting the handle keeps the body
                                    // alive for its lifetime; un-awaited
                                    // would cancel via TaskHandle::Drop.
                                    let _ = handle.await;
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(EngineError::Task(e)));
                                }
                            }
                        });
                    }

                    Control::KillTask { id, signal, reply } => {
                        let kill_timeout = match signal {
                            KillSignal::Kill => Duration::from_millis(0),
                            KillSignal::Term => Duration::from_secs(2),
                        };
                        let engine = internals.clone();
                        // Reply immediately; the cancel ladder runs detached.
                        // Callers that need to observe completion watch the
                        // graph snapshot for `TaskStatus::Cancelled`.
                        let _ = reply.send(Ok(()));
                        tokio::spawn(async move {
                            engine.cancel_subtree_with(id, kill_timeout).await;
                        });
                    }

                    Control::KillAll { reply } => {
                        let engine = internals.clone();
                        let _ = reply.send(Ok(()));
                        // Cancel each direct child of root; root stays alive.
                        tokio::spawn(async move {
                            engine.kill_all().await;
                        });
                    }
                }
            }
        }
    }

    // Quit path: walk the graph and cancel everything under root, then
    // return so Engine::shutdown can complete.
    internals.cancel_subtree(root_id).await;
    Ok(())
}
```

`EngineInternals::spawn_child` (see §8) is the canonical spawn primitive. `ctx.run` returns a `TaskBuilder` which calls `engine.spawn_child(self.task_id, ...)` on `.spawn()` / `.await`. The root body uses `spawn_child` directly with `TaskId::ROOT`, so picker-launched tasks (via `EngineSpawnBuilder`) and `ctx.run`-launched tasks go through the same code path.

## 7. Cancellation wiring

(Read the "Cancellation model" section at top first — this section is the implementation surface of that model.)

- **Crate dependency:** add `tokio-util = { version = "0.7" }` to `Cargo.toml`. We use `CancellationToken` only for its signaling primitive (`cancel()` / `cancelled().await`); we do **not** use its parent-child token tree.
- **Token construction:** every `TaskExecution` (root included) is built with `cancellation: CancellationToken::new()`. Tokens are independent. The graph stored in `Engine::table` + `TaskExecution::children` is the source of truth for parent-child relationships.
- **Status constants:** define `const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);` in `src/execution/engine.rs`. Used by both the process-stop step and the post-stop wait step of the cancel ladder.
- **Two engine verbs.** The cancel ladder is wrapped by `cancel_task` (single task) and `cancel_subtree` (walks the graph and calls `cancel_task` on each). Both live on `EngineInternals`:

  ```rust
  impl EngineInternals {
      // ── Verb 1: single-task ladder. ────────────────────────────
      //
      // Used by:
      //   - TaskHandle::Drop (developer-API path)
      //   - cancel_subtree (as the per-node body)
      //
      // Does NOT walk children. Rust's drop chain or the subtree
      // walker is responsible for propagation (see "Two verbs").
      pub async fn cancel_task(&self, id: TaskId) {
          self.cancel_task_with(id, CANCEL_TIMEOUT).await;
      }

      pub async fn cancel_task_with(&self, id: TaskId, kill_timeout: Duration) {
          let Some(exec) = self.lookup(id).await else { return };

          // 1. Signal the token (no-op if no opt-in observer).
          exec.cancellation.cancel();

          // 2. Stop the task's owned process groups.
          //    SIGTERM → kill_timeout → SIGKILL via ctx.stop_all().
          if let Some(ctx) = exec.task_context() {
              ctx.stop_all(kill_timeout).await;
          }

          // 3. Wait CANCEL_TIMEOUT for the body's tokio task to exit.
          let join = exec.task_handle.lock().await.take();
          let abort_handle = exec.abort_handle.clone();  // cheap clone
          if let Some(handle) = join {
              if tokio::time::timeout(CANCEL_TIMEOUT, handle).await.is_err() {
                  // 4. Still alive — abort the tokio task.
                  abort_handle.abort();  // sync, idempotent
              }
          }

          // 5. Mark Cancelled (only if not already terminal).
          let mut s = exec.status.lock().await;
          if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
              *s = TaskStatus::Cancelled;
          }
      }

      // ── Verb 2: subtree walker. ────────────────────────────────
      //
      // Used by:
      //   - Control::KillTask (any signal)
      //   - Control::Quit (against TaskId::ROOT)
      //
      // Walks `children` parent-first so subscribers see status order
      // top-down. Per-node body is cancel_task_with.
      pub async fn cancel_subtree(&self, root: TaskId) {
          self.cancel_subtree_with(root, CANCEL_TIMEOUT).await;
      }

      pub async fn cancel_subtree_with(&self, root: TaskId, kill_timeout: Duration) {
          let mut stack = vec![root];
          while let Some(id) = stack.pop() {
              let kids: Vec<TaskId> = match self.lookup(id).await {
                  Some(exec) => {
                      let cs = exec.children.lock().await;
                      cs.iter().map(|c| c.id).collect()
                  }
                  None => continue,
              };
              stack.extend(kids);
              self.cancel_task_with(id, kill_timeout).await;
          }
      }

      // ── kill_all: zero out the runtime without killing root. ───
      //
      // Used by Control::KillAll. Cancels each DIRECT child of root.
      // Root stays alive. This is "back to zero state," distinct from
      // Quit's full cancel_subtree(ROOT).
      pub async fn kill_all(&self) {
          let direct_children: Vec<TaskId> = match self.lookup(TaskId::ROOT).await {
              Some(root) => {
                  let kids = root.children.lock().await;
                  kids.iter().map(|c| c.id).collect()
              }
              None => return,
          };
          for id in direct_children {
              self.cancel_subtree(id).await;
          }
      }

      // ── timeout_task: sibling of cancel_task that writes Timeout. ──
      //
      // Same ladder as cancel_task_with(id, CANCEL_TIMEOUT) except step 5
      // sets TaskStatus::Timeout instead of TaskStatus::Cancelled. Called
      // by per-task watchdog tokio tasks (see §13).
      pub async fn timeout_task(&self, id: TaskId) {
          let Some(exec) = self.lookup(id).await else { return };
          exec.cancellation.cancel();
          if let Some(ctx) = exec.task_context() {
              ctx.stop_all(CANCEL_TIMEOUT).await;
          }
          let join = exec.task_handle.lock().await.take();
          let abort_handle = exec.abort_handle.clone();
          if let Some(handle) = join {
              if tokio::time::timeout(CANCEL_TIMEOUT, handle).await.is_err() {
                  abort_handle.abort();
              }
          }
          let mut s = exec.status.lock().await;
          if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
              *s = TaskStatus::Timeout;
          }
      }
  }
  ```

  Implementation note: store an `AbortHandle` (cheap clone of `JoinHandle::abort_handle()`) on `TaskExecution` alongside the JoinHandle. Step 4 calls `abort_handle.abort()` which is sync and idempotent.

  **API surface invariant:** `TaskHandle::Drop` calls `cancel_task` only. `Control::KillTask` / `Control::Quit` call `cancel_subtree`. `Control::KillAll` calls `kill_all`. The watchdog (§13) calls `timeout_task`. Reviewers should reject any code path where Drop reaches for `cancel_subtree`.

- **Task body wrapper:** the centralized launch core (`TaskExecution::spawn_body`) does NOT wrap the body in a select. Tasks that want early-out behavior opt in by checking the cancellation themselves. `TaskContext` exposes two forms:

  ```rust
  impl TaskContext {
      /// Sugar — sync bool check. Most common form, replaces `loop { ... }`.
      pub fn cancelled(&self) -> bool {
          self.cancellation.is_cancelled()
      }

      /// Future form for tokio::select! integration. Returns the
      /// underlying CancellationToken's wait future.
      pub fn cancelled_signal(&self) -> tokio_util::sync::WaitForCancellationFuture<'_> {
          self.cancellation.cancelled()
      }

      /// Raw token, for callers that want to clone and pass it elsewhere.
      pub fn cancellation(&self) -> CancellationToken {
          self.cancellation.clone()
      }
  }
  ```

  Usage:

  ```rust
  // Sugar form (most common):
  while !ctx.cancelled() {
      do_work().await?;
  }

  // Future form — composes with select!:
  loop {
      tokio::select! {
          _ = ctx.cancelled_signal() => break,
          ev = w.next() => { /* ... */ }
      }
  }
  ```

  Tasks that don't observe cancellation at all are still fine — the engine's ladder kills their processes, their `ctx.exec(...).await?` calls bubble `Err`, and they unwind naturally.

- **Lookup:** `engine.table` (Arc<Mutex<HashMap<TaskId, Arc<TaskExecution>>>>) is the lookup table. The root's body has an Arc-clone of `EngineInternals` to drive cancel/kill_all/timeout.

- **Engine-external shutdown:** the canonical shutdown is `engine_handle.quit().await`. The binary's signal handler (Ctrl-C in headless or TUI suspend) calls this. The `Engine` owner can also call `engine.cancel()` directly, which fires the root's token (engine-external token kept on `Engine` itself) — that hits the root's `tokio::select!` arm, breaks the loop, and flows into `cancel_subtree(ROOT)`. Both paths converge.

## 8. The seam at `Registry::run_with_registry`

**Today (src/task.rs:894-904):**

```rust
pub async fn run_with_registry(&self, name, args, registry) -> TaskResult {
    let task = self.resolve(name)?;
    let mut ctx = TaskContext::new(task.name);
    ctx.set_registry(registry.clone());
    task.func.call(&ctx, args).await
}
```

**After: the engine owns the spawn primitive. `ctx.run` returns a builder.**

`EngineInternals::spawn_child` is the canonical entry point. Both `TaskBuilder` (returned by `ctx.run`) and `EngineSpawnBuilder` (returned by `EngineHandle::spawn_task`) funnel through it. Future MCP/headless code paths can call `engine.spawn_child(parent_id, ...)` directly without needing a `TaskContext`.

```rust
impl EngineInternals {
    /// Register and launch a child of `parent_id` running task `def`
    /// with `args` and per-invocation `opts`. Returns the handle
    /// synchronously after the body's tokio task is spawned. Sets up
    /// the timeout watchdog if `opts.timeout.is_some()` (see §11).
    pub fn spawn_child(
        self: &Arc<Self>,
        parent_id: TaskId,
        def: &'static TaskDef,
        args: Vec<String>,
        opts: SpawnOptions,
    ) -> Result<TaskHandle, TaskError> {
        let id = TaskId::next();
        let exec = Arc::new(TaskExecution::new_child(NewChildArgs {
            id,
            parent: Some(parent_id),
            task_name: def.name.to_string(),
            cancellation: CancellationToken::new(), // independent, NOT a child token
            log_store: self.log_store.clone(),
            registry: self.registry.clone(),
        }));

        // Register before launching so the graph snapshot reflects the
        // new node before any output appears. register_task also pushes
        // `exec` onto parent.children.
        self.register_task(exec.clone(), parent_id);

        // Build the child TaskContext (mirrors TaskExecution::launch today).
        let ctx = TaskContext::new_for(exec.clone(), Arc::downgrade(self));

        // Spawn the body. spawn_body stores the JoinHandle and AbortHandle
        // on `exec`, and (if opts.timeout is Some) registers a watchdog
        // tokio task (see §11).
        TaskExecution::spawn_body(self, exec.clone(), def, args, ctx, opts);

        Ok(TaskHandle::new(exec, Arc::downgrade(self)))
    }
}
```

`ctx.run` returns a `TaskBuilder` (lazy). The builder mirrors `EngineSpawnBuilder` and `SpawnBuilder` (for processes) — same shape, same `.timeout()` API:

```rust
impl TaskContext {
    /// Configure a child task. Returns a builder; `.timeout(d)`,
    /// `.spawn()`, or `.await` to fire it. Default await semantics
    /// (via IntoFuture): runs the task to completion, resolves to its
    /// `TaskResult`. (Asymmetric vs `engine.spawn_task().await` which
    /// resolves to the new TaskId — see §5.)
    pub fn run(&self, name: &str, args: &[&str]) -> TaskBuilder {
        // Resolution happens here so errors surface synchronously rather
        // than at .spawn()/.await time. The task_def lookup is cheap.
        let registry = match self.registry.as_ref() {
            Some(r) => r.clone(),
            None => return TaskBuilder::failed(TaskError::from_display("no registry available")),
        };
        let task_def = match registry.resolve(name) {
            Ok(d) => d,
            Err(e) => return TaskBuilder::failed(e),
        };
        TaskBuilder {
            inner: Ok(TaskBuilderInner {
                parent_id: self.task_id,
                engine: self.engine.clone(),
                task_def,
                args: args.iter().map(|s| s.to_string()).collect(),
                timeout: None,
            }),
        }
    }
}

/// Builder returned by `ctx.run`. Lazy — nothing is spawned until
/// `.spawn()` is called or the builder is awaited.
///
/// Drop without spawn = no-op (nothing was launched). This is correct:
/// the builder is a configuration value, not a lifetime token. The
/// drop-cancels semantics belong to `TaskHandle`, returned by .spawn().
pub struct TaskBuilder {
    inner: Result<TaskBuilderInner, TaskError>,
}

struct TaskBuilderInner {
    parent_id: TaskId,
    engine: Weak<EngineInternals>,
    task_def: &'static TaskDef,
    args: Vec<String>,
    timeout: Option<Duration>,
}

impl TaskBuilder {
    fn failed(e: TaskError) -> Self { Self { inner: Err(e) } }

    pub fn timeout(mut self, d: Duration) -> Self {
        if let Ok(ref mut inner) = self.inner { inner.timeout = Some(d); }
        self
    }

    /// Spawn the task body and return the handle. Eager.
    pub fn spawn(self) -> Result<TaskHandle, TaskError> {
        let inner = self.inner?;
        let engine = inner.engine.upgrade()
            .ok_or_else(|| TaskError::from_display("no engine context"))?;
        let opts = SpawnOptions { timeout: inner.timeout };
        engine.spawn_child(inner.parent_id, inner.task_def, inner.args, opts)
    }
}

impl IntoFuture for TaskBuilder {
    type Output = TaskResult;
    type IntoFuture = Pin<Box<dyn Future<Output = TaskResult> + Send>>;

    /// Default await: spawn the task and await its completion.
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let handle = self.spawn()?;
            handle.await
        })
    }
}

impl Registry {
    /// Backward-compatible entry point. Funnels through the builder.
    pub async fn run_with_registry(
        &self,
        parent_ctx: &TaskContext,
        name: &str,
        args: &[String],
        _registry: &Arc<Registry>,
    ) -> TaskResult {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        parent_ctx.run(name, &arg_refs).await
    }
}
```

`TaskExecution::spawn_body` is the reusable launch core: takes an `Arc<EngineInternals>`, an `Arc<TaskExecution>`, the task def, args, a configured `TaskContext`, and the `SpawnOptions`. It spawns the tokio task running the body, stores the `JoinHandle` and `AbortHandle` on `exec`, and (if `opts.timeout.is_some()`) registers the timeout watchdog. The synthetic root, picker-launched tasks (via `EngineSpawnBuilder` → `Control::SpawnTask` → root body → `spawn_child`), and every `ctx.run` child all go through it.

**Asymmetry note (call out for implementors):** `ctx.run("foo").await` runs the task to completion and yields `TaskResult`. `engine.spawn_task(def, args).await` registers the task and yields `Ok(TaskId)` — it does NOT wait for completion. This matches the typical call-site needs (in-task composition wants the result; TUI/MCP wants the id to track). If a future caller wants "spawn from engine handle and await completion," add `EngineSpawnBuilder::run() -> impl Future<Output = TaskResult>` then; not needed for slice 4.

**New `TaskContext` fields** required:

- `task_id: TaskId` — identity of the running task
- `cancellation: CancellationToken` — clone of the running task's own token; exposed via `ctx.cancelled()` / `ctx.cancelled_signal()` / `ctx.cancellation()` (see §7)
- `engine: Weak<EngineInternals>` — for `run` (via TaskBuilder) and for `TaskHandle::Drop`'s ladder

`engine` is `Weak` to break the cycle (Engine owns Arc<TaskExecution> which references TaskContext; TaskContext→Engine via Weak avoids retention). `cancellation` is **the running task's own** token (not a parent reference) — child tasks created via `ctx.run` get freshly minted independent tokens (see "Cancellation model").

## 9. Log routing

**Single engine-owned `LogStore`. All tasks and processes push into it; the source key is an integer `TaskId`.**

Decision: log source keys are numeric `TaskId` values, not strings. **Tasks and processes share an ID namespace** — every spawned process is assigned a `TaskId` from the same `TaskId::next()` allocator. The flat engine table (`HashMap<TaskId, ...>`) holds both kinds, and the graph represents processes as children of tasks (`TaskNode::processes: Vec<ProcessInfo>`, where each `ProcessInfo` carries `id: TaskId`).

Implications:

- `LogStore`'s source-key field changes from `String` to `TaskId` (or a renamed `Id`/`SourceId` newtype if you prefer; **flagged for confirmation** — the user said "same ID system," which I'm reading as "use `TaskId` directly." If they meant "rename to a neutral `Id`," that's a small but real follow-up.).
- Display rules (how `t7` or `cargo build` renders in the sidebar / log header) live in the UI layer, not the storage layer. The engine never produces display strings.
- Duplicate-name disambiguation (two `cargo build` tasks) is automatic: their IDs differ.

**Filtering for a subtree (e.g. parent + descendants):**

```rust
impl EngineHandle {
    /// All source IDs belonging to this task or any descendant
    /// (tasks AND processes). Useful when the TUI focuses a non-leaf
    /// task and wants to filter logs.
    pub async fn source_ids_for(&self, id: TaskId) -> Vec<TaskId> {
        // Walk graph snapshot, return [id, ...descendant task ids,
        // ...all processes owned by those tasks]. Pass result to
        // LogStore::output_for_many.
    }
}
```

A new `LogStore::output_for_many(&[TaskId])` method (or rename of an existing aggregator) is added in slice 3. The existing `output_for(source)` becomes `output_for(id: TaskId)`.

Process output (stdout/stderr from child processes) flows through the existing `monitor_spawns` loop into the shared `LogStore` keyed by the **process's own** `TaskId`. The TUI's task-level filtering uses `source_ids_for(task_id)` which includes both the task's id and its child process ids.

## 10. Slice boundaries — sanity check

Re-checked against the four engine slices in the plan:

| Slice | Stated content | Buildable with these types? |
|---|---|---|
| 1 | `Control` enum, root TaskDef, root body select loop. SpawnTask = `ctx.run(name, args).await` (no graph yet). KillTask stub. | **Yes.** Slice 1 builds `Control`/`KillSignal`/`root.rs`. The root body's SpawnTask arm calls the existing (non-graph-aware) `ctx.run`. KillTask is a logging stub. The `Engine` type isn't required — slice 1 can wire the root through a temporary entry point invoked from the existing TUI runner or test. |
| 2 | Recursive `TaskExecution` + `TaskId`. Move LogStore ownership. No behavior change. | **Yes.** Adds `TaskId`, `parent`, `children`, `cancellation` to `TaskExecution`. Engine-owned LogStore appears here; the existing `TaskRunner` adapts to receive an `Arc<Mutex<LogStore>>` from outside. Single-task path still works because the `parent`/`children` fields default to None/empty. |
| 3 | `ctx.run()` graph-aware via `TaskBuilder` + `TaskHandle` with drop-cancels. | **Yes.** The seam rewrite (§8) lands here. `TaskBuilder` (returned by `ctx.run`), `TaskHandle`, and the new `TaskContext` fields all land here. `TaskBuilder::timeout()` is wired but inert until slice 4 (the watchdog lives there). `EngineInternals::spawn_child` lands here too — it's the canonical primitive both builders feed. |
| 4 | `Engine::start()`, method-based `EngineHandle`, cancel ladder, `KillAll`, timeouts, headless + signal handler. | **Yes.** The `Engine` struct + `EngineHandle` (method API) + `EngineSpawnBuilder` + `SpawnOptions` + `GraphSnapshot` watch + cancel ladder + `kill_all` + `timeout_task` + watchdog wiring all land here. Headless `runme <task>` is rewritten to call `engine_handle.spawn_task(def, args).timeout(d).await`, await its TaskId's status, then `engine_handle.quit()`. The binary registers a SIGINT/SIGTERM handler that calls `engine_handle.quit().await` and parses `--timeout`. |

**Slicing notes:**

- The cancel ladder lives on `EngineInternals` (`cancel_task` / `cancel_subtree` / `kill_all` / `timeout_task`), so it lands in slice 4 with the rest of the engine. Slice 3's `TaskHandle::Drop` therefore falls back to `exec.cancellation.cancel()` (signal-only) for tests; slice 4 swaps in the full ladder via `engine.cancel_task(id)`. Document this in slice 3's PR description; tests in slice 3 verify token-fires-on-drop, not full ladder teardown.
- Slice 3 lands `TaskBuilder` (returned by `ctx.run`) including its `.timeout()` setter. The setter populates `SpawnOptions::timeout` but no watchdog runs until slice 4 wires `spawn_child` to consult it. Tests in slice 3 verify the option flows through; tests in slice 4 verify the watchdog actually fires.
- Slice 4 lands `EngineSpawnBuilder`, `SpawnOptions`, the timeout watchdog (`watchdog_abort` field on `TaskExecution`, watchdog spawn in `spawn_child`, abort in `cancel_task_with`), and the CLI `--timeout` flag plumbing.
- `TaskStatus::Cancelled` and `TaskStatus::Timeout` both land in slice 2 alongside the rest of the status enum reshape (no consumers until slices 3/4 respectively).

## 11. Timeouts

Per-task timeouts ride on top of the cancellation infrastructure. They are in scope for slice 4 — building them now stress-tests the cancel ladder and catches design holes before they become rework.

### Per-invocation, not per-TaskDef

Timeouts are configured **at the call site**, mirroring `ctx.spawn(...).timeout(...)` for processes. There is **no `TaskDef::timeout` field** — different callers may want different timeouts for the same task definition.

Three call sites:

| Site | Form |
|---|---|
| In task code | `ctx.run("foo", &[]).timeout(d).await?` |
| TUI / MCP via engine handle | `engine.spawn_task(def, args).timeout(d).await?` |
| CLI | `rnme --timeout 30s build` — the binary parses the flag and calls `engine.spawn_task(def, args).timeout(d)` before awaiting |

The setting flows through `SpawnOptions::timeout: Option<Duration>` (defined in §4). `EngineInternals::spawn_child` consumes `SpawnOptions`; both `TaskBuilder` and `EngineSpawnBuilder` populate it from their `.timeout()` setter.

### Watchdog wiring

`EngineInternals::spawn_child` (§8) inspects `opts.timeout`. If `Some(d)`, it spawns a watchdog tokio task:

```rust
// Inside spawn_child, after spawn_body:
if let Some(d) = opts.timeout {
    let engine = self.clone();
    let id = exec.id;
    let watchdog = tokio::spawn(async move {
        tokio::time::sleep(d).await;
        engine.timeout_task(id).await;
    });
    // Store the watchdog's AbortHandle on `exec` so cancel_task can
    // abort it; otherwise a cancelled task would still get a delayed
    // Timeout overwrite.
    *exec.watchdog_abort.lock().await = Some(watchdog.abort_handle());
}
```

`TaskExecution` gains a `watchdog_abort: Mutex<Option<AbortHandle>>` field for this.

### `cancel_task_with` aborts the watchdog

Step 1 of the cancel ladder grows a sub-step: abort the watchdog if any. This prevents a Cancel→Timeout race:

```rust
// In cancel_task_with, before step 1:
if let Some(h) = exec.watchdog_abort.lock().await.take() {
    h.abort();
}
// ... rest of ladder unchanged
```

### `TaskStatus::Timeout` is a real variant

Already committed in §2's migration table and in the Cancellation model's `TaskStatus` snippet. The full enum:

```rust
pub enum TaskStatus {
    Setup,
    Ready,
    Done,
    Failed(TaskFailure),
    Cancelled,
    Timeout,
}
```

`TaskHandle.await` for a timed-out task resolves to `Err(TaskError::timeout())` (mirroring the `Cancelled` recovery path in §3). The TUI renders `Timeout` with its own status color in `task_status_display`.

### CLI integration (slice 4)

The `rnme` binary parses `--timeout <humantime>` and threads the value into the engine call:

```rust
// In src/bin/rnme/main.rs (or wherever the headless launch lives):
let mut builder = engine_handle.spawn_task(def, args);
if let Some(d) = cli.timeout {
    builder = builder.timeout(d);
}
let task_id = builder.await?;
// ... await completion via graph snapshot or status subscription
```

### Why it stress-tests cancellation

The watchdog exercises:

- Aborting the watchdog when a task is cancelled before its timeout fires (otherwise `Cancelled` gets overwritten by a delayed `Timeout`).
- The full cancel ladder running on a task whose body is "stuck" (timeouts often happen because a task is hung) — including the abort step, since timed-out tasks may not respond to token signaling.
- The same ladder semantics for distinct triggers (Drop, KillTask, KillAll, Quit, Timeout) — if any of them behave differently, the abstraction is wrong.

If implementation surfaces a hole in this story, escalate. The whole point of doing timeouts now is to catch those before slice 4 ships.

## 12. New decisions (not in design doc)

1. **Module home:** `src/execution/` directory module rather than a single file or scattering across `task.rs`. (§0)
2. **`TaskId(0) = ROOT`:** the synthetic root has a fixed const ID, allocator starts at 1. Lets tests and the root's body name the root without lookup.
3. **TaskId allocator is module-static, not engine-owned:** simplifies generation in code paths that only have a `TaskContext`.
4. **`LogStore` is engine-singleton, source-keyed by integer `TaskId`:** single store; tasks and processes share an ID namespace; display strings live in the UI. (§9)
5. **`ctx.run` returns a `TaskBuilder`** (lazy, mirrors `SpawnBuilder` for processes). `.timeout()`, `.spawn()`, or `.await` to fire it. Default await semantics: run to completion, yield `TaskResult`. Drop without spawn = no-op. (§8)
6. **`TaskContext` gains 3 fields** (`task_id`, `cancellation`, `engine: Weak`): they're load-bearing for the seam in §8 and don't have a non-API-changing alternative. (§8)
7. **Graph reader = `watch::Receiver<GraphSnapshot>`** with immutable `Arc<HashMap>`-backed snapshot. Chosen over `Arc<RwLock<Graph>>` to match the TUI's immediate-mode preference and avoid lock contention. (§5)
8. **Synthetic root lives in `src/execution/root.rs`, not `src/builtin.rs`:** root must be invisible to the user-facing catalog. The plan's draft was wrong on this; flagging for plan update.
9. **Task body wrapper is centralized in `TaskExecution::spawn_body`:** all task bodies (root and children) get cancel-on-token semantics for free. (§7)
10. **`TaskResult` from JoinHandle**: the JoinHandle's payload type changes from `()` to `TaskResult`, so the handle's `.await` can return the result without a separate channel. (§2)
11. **Cancellation tokens are independent** (`CancellationToken::new()`); we do NOT use `child_token()`. Propagation has two distinct rules: developer-API (`Drop`) propagates via Rust's drop chain; explicit user actions (`kill_task`, `kill_all`, `quit`) propagate via `EngineInternals::cancel_subtree` / `kill_all`. ("Cancellation model" section, "Two verbs")
12. **Cancellation is opt-in for task bodies**: most tasks don't observe the token. The engine's cancel ladder (token → `stop_all` → 2s wait → abort → `Cancelled`) does the work. (Cancellation model)
13. **`TaskStatus::Cancelled` is its own variant**, sibling of `Failed`/`Done`/`Timeout`. (Cancellation model + §2 migration table)
14. **Err completion does NOT cancel children** (settled: option A). Children survive their parent's error unless the parent explicitly drops/cancels their handles. (Cancellation model)
15. **`TaskHandle::Drop` cancels ONE task, not the subtree.** Drop spawns a tokio task calling `engine.cancel_task(id)`. Rust's drop chain handles propagation: on-stack child handles drop with the parent's stack and fire their own ladders; detached children (in `tokio::spawn`) are intentionally spared. This is the developer-API path. (§3, "Two verbs" in Cancellation model)
16. **Engine exposes two cancellation verbs**: `cancel_task(id)` (single-task ladder) and `cancel_subtree(id)` (walks the graph, calls `cancel_task` per node). `Drop` uses the first; `Control::KillTask` and `Control::Quit` use the second. The split is the load-bearing distinction between developer-API and explicit-user-action propagation. (§7)
17. **`AbortHandle` stored alongside the JoinHandle** so the abort step of the cancel ladder works without re-locking. (§7)
18. **Public API is method-based on `EngineHandle`**, not raw `Control`. `Control` is `pub(crate)`; methods (`spawn_task`, `kill_task`, `kill_all`, `quit`) serialize into Control messages with oneshot replies. Keeps the public surface stable as the protocol evolves. (§4, §5)
19. **Engine owns the spawn primitive** (`EngineInternals::spawn_child`, taking `SpawnOptions`); `TaskBuilder` (from `ctx.run`) and `EngineSpawnBuilder` (from `EngineHandle::spawn_task`) both feed it. Future MCP/headless callers can use `spawn_child` directly without a `TaskContext`. (§8)
20. **`Control::KillAll` is its own variant** (not `KillSignal::All`). Semantics: cancel each direct child of root, root stays alive — "back to zero state," not shutdown. Distinct from `Quit` which cancels the entire ROOT subtree and exits the runtime. (§4, §6, §7)
21. **`KillSignal` reduces to `Term` and `Kill`** (no `All` variant). (§4)
22. **Log source keys are integer `TaskId`s** in a unified ID space shared by tasks and processes. Display formatting lives in the UI layer. (§9)
23. **`ctx.cancelled()` sugar** (sync bool) plus `ctx.cancelled_signal()` (future) plus `ctx.cancellation()` (raw token). Three forms covering the common cases. (§7)
24. **Signal handler in slice 4:** the binary registers a SIGINT/SIGTERM handler that calls `engine_handle.quit().await` for clean Ctrl-C teardown. (§10)
25. **Per-task timeouts are per-invocation, not per-TaskDef.** Configured at the call site via `.timeout(d)` on the builder (mirroring process spawn). Three call sites: `ctx.run().timeout()`, `engine.spawn_task().timeout()`, and the CLI `--timeout` flag. Threads through `SpawnOptions::timeout` into `EngineInternals::spawn_child`, which spawns a watchdog tokio task. `EngineInternals::timeout_task` writes `TaskStatus::Timeout`. The watchdog is aborted by `cancel_task_with` to prevent Cancel→Timeout races. (§11)
26. **`TaskStatus::Timeout` is a real variant** (not "if/when added"). Sibling of `Cancelled` and `Failed`. Lands in slice 2 alongside `Cancelled`. (§11, §2 migration table)
27. **`SpawnOptions` is the per-invocation config struct** consumed by `EngineInternals::spawn_child`. Designed to grow (future: ready_when, env overlay) without churning call sites. (§4, §8)
28. **`EngineHandle::spawn_task` returns `EngineSpawnBuilder`** mirroring `TaskBuilder`. Default await semantics differ: `engine.spawn_task().await` resolves to `Ok(TaskId)` (registered, not completed); `ctx.run().await` resolves to `TaskResult` (completed). Documented asymmetry — matches the typical call-site needs. (§5, §8)

## 13. Open questions for human review

**None.** All prior follow-ups are resolved:

- `TaskId` stays as-is (no rename for the unified ID space).
- Timeouts are per-invocation, configured via builder; no `TaskDef::timeout` field, so no macro syntax decision.

The doc is final. Slice 1 may proceed.
