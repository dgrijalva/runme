# Research: TaskContext::run / Registry::run_with_registry Flow

## Full Call Graph: ctx.run("foo", args) → execution

**Entry point:** `TaskContext::run()` at `src/task.rs:659`

```
ctx.run("foo", &["arg1", "arg2"])  (line 659)
  ↓
Registry::run_with_registry(name, &string_args, registry)  (line 665)
  ↓
Registry::resolve(name)  (line 900)
  ↓
TaskDef lookup via group/name/short rules  (lines 916-973)
  ↓
TaskContext::new(task.name)  (line 901)
  ↓
task.func.call(&ctx, args)  (line 903)
  ↓
User's async fn body executes
```

**Each step's ownership:**

| Step | File:Line | Owns What | State Carried |
|------|-----------|-----------|---------------|
| `ctx.run()` | task.rs:659-665 | Calling task's context | registry Arc only |
| `run_with_registry()` | task.rs:894-903 | Execution context creation | Nothing injected yet |
| `TaskContext::new()` | task.rs:201-217 | Fresh context | output buffer, pgids vec, spawn_tx=None, registry, task_status=None |
| `task.func.call()` | task.rs:93-101 | Function dispatch | Returns Pin<Box<Future>> |
| User fn body | various | Task body | Full ctx with all initialized fields |

## TaskContext Construction for Child Invocation (line 901)

**Fresh TaskContext created with:**
- `name` = task.name (only field explicitly set)
- `output` = new Arc<Mutex<OutputBuffer>> (10K capacity) — line 206
- `tracing_output` = None (not injected in `run_with_registry`)
- `spawned_pgids` = new Arc<Mutex<Vec>>
- `spawn_tx` = None (no spawn notification in run_with_registry path)
- `tui_wait` = Arc<AtomicBool::new(true)>
- `tui_output` = new Arc<Mutex<TuiOutput>>
- `watches` = empty
- `watch_dir` = current dir
- `registry` = set to caller's registry (line 902) ✓
- `task_status` = None (NOT SET in run_with_registry)

**Missing state that exists in TaskExecution::launch context:**
- No `tracing_output` buffer (no structured logging routing)
- No `task_status` shared Arc (no observable status)
- No `spawn_tx` hooked to a spawn monitor (spawned processes untracked)
- No parent reference

## The Seam: Where Graph-Aware Wrapping Should Happen

**Current state (line 894-903):**
```rust
pub async fn run_with_registry(
    &self,
    name: &str,
    args: &[String],
    registry: &Arc<Registry>,
) -> Result<(), TaskError> {
    let task = self.resolve(name)?;
    let mut ctx = TaskContext::new(task.name);
    ctx.set_registry(registry.clone());
    task.func.call(&ctx, args).await  // ← SEAM: No TaskExecution wrapper
}
```

**The seam is at line 903.** Before `task.func.call()`, the code should:
1. Create a `TaskExecution` child (with parent reference to caller)
2. Call `TaskExecution::launch()` instead of `task.func.call()` directly
3. Return a `TaskHandle` wrapping the child's lifecycle token
4. Wire up the child's status/log/spawn notifications to the task graph

**Why here and not elsewhere:**
- `run()` (line 874-876) just delegates to `run_with_registry` — one level of indirection
- `TaskExecution::launch()` (line 276-349) is where TUI mode does full setup; same pattern applies here
- `run_with_registry` is the **only** entry point for registry-aware task invocation (enabling transitive calls)
- All other launch paths (`run_with_args` line 879, CLI/TUI) go through `TaskExecution::launch` separately

## TaskContext vs TaskExecution Relationship Today

**They are independent concerns:**

- **`TaskContext`** (task.rs:150-181): Task function's runtime API. Passed to `task.func.call()`. Owns:
  - Process/spawn tracking (pgids, spawn_tx)
  - Output buffers (exec, tracing, tui_output)
  - Registry reference
  - Lifecycle signals (tui_wait flag)

- **`TaskExecution`** (execution.rs:185-232): Task lifecycle manager. Owns:
  - `TaskContext` creation and injection
  - LogStore (consolidated log output)
  - TaskStatus Arc (shared with ctx via `set_task_status`)
  - Process monitoring loop (spawn_rx)
  - JoinHandle to spawned tokio::task

**No parent-child relationship exists today.** `TaskContext` has no reference to a parent task, and `TaskExecution` has no reference to who invoked it.

## tasks() Query Side (line 682-686)

```rust
pub fn tasks(&self) -> Option<TaskQuery> {
    self.registry.as_ref().map(|r| TaskQuery {
        registry: r.clone(),
    })
}
```

Returns a query handle wrapping the registry. `TaskQuery` (lines 729-766) provides:
- `.all()` — lists all registered tasks
- `.matching(pattern)` — glob-matches qualified names (`"group:name"`)

**Wiring:** Query side is decoupled; no graph involvement. Pure registry lookup.

## Spawn Primitives on TaskContext

| Method | Line | Returns | Purpose |
|--------|------|---------|---------|
| `spawn()` | 402-444 | SpawnBuilder | Configure + spawn long-running process |
| `exec()` | 371-373 | ProcessResult | Synchronously run command (sugar for spawn().complete()) |
| `stop_all()` | 455-475 | () | Signal all spawned process groups |
| `bind_ready()` | 615-627 | () | Watch process readiness, update task status |
| `mark_ready()` | 633-642 | () | Manually mark task ready |
| `set_spawn_notifier()` | 240-242 | () | Wire spawn_tx sender (TUI only) |

**Symmetry note for TaskHandle design:** These are all async/RAII primitives. `ProcessHandle` (process.rs:~292+) is similar — owns child process, auto-signals group on drop/stop, holds JoinHandles for output tasks. `TaskHandle` should mirror this: own the child `TaskExecution`, signal cancellation on drop, expose `.await` for result.

## Blocking Issues for ctx.run() → TaskHandle

| Issue | Scope | Resolution |
|-------|-------|-----------|
| Lifetime coupling | Parent ctx must outlive child | Parent spawned in root, child can outlive parent (design decision 3: completed tasks stay) |
| `Send + Sync` constraints | TaskHandle must cross await boundaries | TaskExecution is Arc-based, fully Send/Sync; no issue |
| Result type | Today returns `TaskResult` directly; handle needs IntoFuture | IntoFuture trait + async block: `.await` → TaskResult (line 110 of design doc) |
| Cancellation integration | Drop semantics need async cancellation | Use `tokio_util::CancellationToken` (per researcher-cancellation) |
| Parent reference | Child needs to know parent for graph | Add `parent_id: TaskId` field to TaskExecution |
| Log routing | Child output must reach parent's LogStore | Use shared LogStore or parent-aware routing in spawn monitor |

**No hard blockers.** Design doc already accounts for these (lines 108-125).

## Key Call-Graph Facts

1. **Single entry point for graph injection:** `Registry::run_with_registry()` line 894. All `ctx.run()` calls funnel here.
2. **No tracing setup in run_with_registry:** Unlike `TaskExecution::launch()` (line 289-324), no LogEntryLayer installed. Child task's `info!()` calls go to stderr unstructured.
3. **No spawn monitoring:** Child's `ctx.spawn()` calls have `spawn_tx=None`, so spawned processes never emit `SpawnEvent`. They run but aren't visible to the TUI runner.
4. **Registry is injected transitively:** Line 902 passes caller's registry clone. Enables N levels of `ctx.run("a").await?.run("b").await` chains.
5. **Status tracking broken:** Child's `ctx.bind_ready()` (line 616) tries to lock `self.task_status` which is `None` in run_with_registry path. No-op.
6. **Output buffers are isolated:** Child's exec/spawn output goes to its own OutputBuffer, never forwarded to parent's LogStore. Lost on child completion.
7. **TaskExecution owns the graph machinery:** All the graph-aware setup (status/log/spawn/monitoring loops) lives in `TaskExecution::launch()`. `run_with_registry` bypasses it.
