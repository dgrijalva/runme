# Research: TaskExecution Lifecycle

## TaskExecution Structure (src/execution.rs:185-205)

**Core fields:**
- `task_handle: Option<tokio::task::JoinHandle<()>>` — holds the spawned task function (created line 328, awaited in wait(), never explicitly aborted)
- `log_store: Arc<Mutex<LogStore>>` — shared output aggregation (can be created fresh or shared via with_log_store)
- `processes: Arc<Mutex<Vec<ProcessInfo>>>` — flat list of spawned subprocesses
- `spawn_tx: mpsc::UnboundedSender<SpawnEvent>` — sender for process spawn notifications
- `task_status: Arc<Mutex<TaskStatus>>` — current state (Setup → Done/Failed or Ready)
- `tracing_installed: Arc<AtomicBool>` — global subscriber flag (process-wide, shared)
- `registry: Option<Arc<Registry>>` — optional, enables ctx.run()

**Ownership:** All state is Arc-wrapped for shareability. No parent pointer, no children list, no TaskId.

## How TaskExecution::launch() Wires the Task Body (src/execution.rs:276-349)

1. **Lines 284-305:** Creates TaskContext, injects dependencies (spawn notifier, status Arc, registry, tui_wait/tui_output)
2. **Lines 307-318:** Wires output forwarding:
   - `start_buffer_forwarder()` spawns tokio task to forward exec output → LogStore
   - Subscribes to tracing buffer BEFORE installing subscriber (avoids message loss)
   - `start_tracing_forwarder()` spawns tokio task to forward tracing logs → LogStore
3. **Lines 326-348:** Spawns the actual task:
   ```rust
   tokio::spawn(async move {
       let result = task.func.call(&ctx, &task_args).await;  // ← user's fn here
       // Update task_status (Setup → Done/Failed)
   });
   ```

**Key insight:** Task function is called directly, NOT wrapped in TaskExecution. For multi-task, ctx.run() must create a child TaskExecution before invoking the child task's function.

## SpawnEvent Routing (src/task.rs:402-443, src/execution.rs:441-502)

**Sender:** `ctx.spawn(cmd)` creates SpawnBuilder with on_spawn callback that captures `spawn_tx`. When process starts, callback sends SpawnEvent including:
- `buffer: Arc<Mutex<OutputBuffer>>` — process's output
- `pgid`, `pid` — process identifiers
- `command_label`, `task_name` — display metadata
- `readiness_rx: Option<tokio::sync::watch::Receiver<bool>>` — if readiness condition configured

**Receiver & forwarder:** `TaskExecution::monitor_spawns()` background loop (line 217-219 spawn, lines 441-502 loop):
1. Receives SpawnEvent
2. Creates ProcessInfo(status=Running), appends to processes Vec
3. Watches readiness condition if present → updates proc.ready
4. **Crucially:** Subscribes to process OutputBuffer and spawns forwarder tokio task that pushes entries to LogStore

**Channel topology:**
- Unbounded mpsc: spawn_tx sender (in TaskContext) → spawn_rx receiver (in monitor_spawns loop)
- Multiple background tokio tasks forward OutputBuffer → LogStore via broadcast

## LaunchConfig: tui_wait and tui_output (src/execution.rs:157-175)

**Two fields being removed (design decision 7):**
- `tui_wait: Option<Arc<AtomicBool>>` — default true; if false, TUI auto-exits after task
- `tui_output: Option<Arc<Mutex<TuiOutput>>>` — post-TUI output staging buffer

**Flow:** Created in LaunchConfig → injected via ctx.set_tui_wait/set_tui_output → TaskContext reads/updates via ctx.tui_wait() → TUI event loop checks flag.

**Current usage:**
- TaskRunner::launch (src/tui/runner.rs:100-103) sets both
- Registry::run_with_args and headless use LaunchConfig::default() (both None)

**Removal:** Both fields are niche TUI concerns. Can drop entirely — no users outside TUI, pre-release software.

## JoinHandle Ownership & Lifetime (src/execution.rs:195, 328, 354-358)

**Task function JoinHandle:**
- Created in launch() line 328: `tokio::spawn(async move { task.func.call(...).await })`
- Stored in `task_handle: Option<JoinHandle<()>>`
- Awaited in wait() (lines 354-358) via .await, consuming handle
- Never explicitly aborted in current code
- When drop without await → process continues running (for multi-task detachment, must signal cancellation token separately)

**Background JoinHandles (not stored):**
- monitor_spawns spawned at line 217-219
- Output forwarders spawned at lines 117, 140, 490
- Readiness watchers spawned at line 473

These are fire-and-forget; they terminate when channels close or inner loops exit.

## Shutdown Propagation (src/execution.rs:360-408)

**Entry point:** TaskExecution::shutdown(timeout: Duration)

**Mechanism:**
1. Collect all running process groups from processes Vec
2. Send SIGTERM to each pgid via nix::signal::killpg
3. Poll until all exit or timeout
4. Send SIGKILL to survivors

**Obs:** Shutdown uses subprocess signals, not task cancellation. For multi-task:
- Root's Control::Quit must trigger cascade
- Control::KillTask for individual task must use cancellation token (not subprocess signals — tasks aren't processes)

## Existing Graph-Shaped Pieces

**Limited today:**
- `processes: Vec<ProcessInfo>` — flat list, no hierarchy
- `registry: Option<Arc<Registry>>` — enables ctx.run() but doesn't track children created
- monitor_spawns loop — already routes events through channel; natural seam for task-spawn events

**Missing for multi-task:**
- No TaskId assignment (design says monotonic)
- No parent reference in TaskExecution
- No children list in TaskExecution
- No task history (only current status; completed tasks reset)

## Integration Seam for Graph-Aware ctx.run()

**Call stack (src/task.rs:659-666, 894-904):**
1. `ctx.run(name, args)` → calls registry.run_with_registry
2. Registry::run_with_registry resolves task, creates TaskContext, calls task.func.call

**For multi-task:** After resolve(), before task.func.call(), create child TaskExecution with:
- Parent ID/pointer
- Unique TaskId
- Shared LogStore
- Shared cancellation token
Return TaskHandle (IntoFuture + Drop-cancels) instead of direct result
