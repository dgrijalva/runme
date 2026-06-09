# Plan: Output Unification, TUI Behavior, Watch API

## Goal

Implement three design changes from `docs/system_design.md`:

1. **Output unification** — Replace `ExecOutput` with a unified `Output` type backed by `OutputBuffer`. Introduce `ProcessResult` with `.ok()` for `?` ergonomics.
2. **TUI behavior control** — `ctx.tui_wait(bool)` and `ctx.tui_output()` with append/subscribe for post-TUI output.
3. **Watch API** — `Watch<T>` type with `ctx.watch()`, `ctx.watch_with()`, `ctx.watch_channel()`, and `glob_filter` utility.

## Approach

Sequential phases because `task.rs` and `process.rs` are modified by all three features. Phase 1 (Output) is foundational. Phases 2 and 3 (TUI, Watch) run in parallel worktrees after Phase 1 lands.

## Acceptance Criteria

- [x] `ExecOutput` struct is removed; all callers use `Output` or `ProcessResult`
- [x] `Output` type exposes `.entries()`, `.subscribe()`, `.stdout()`, `.stderr()`
- [x] `ctx.exec()` returns `ProcessResult` with `.ok()` returning `Result<ProcessResult, ProcessResult>`
- [x] `ProcessHandle` exposes `.output() -> Output` (replaces public `buffer` field)
- [x] `ctx.tui_wait(false)` causes TUI to auto-exit on task completion
- [x] `ctx.tui_output()` returns `TuiOutput` with `.append(&Output)` and `.subscribe(&Output)` on both stdout/stderr streams
- [x] Post-TUI output is flushed to real stdio after `restore_terminal()`
- [x] `ctx.watch("glob")` returns `Watch<Vec<PathBuf>>` with `.next().await` and `.label()`
- [x] `ctx.watch_with(f)` returns `Watch<T>` with generic filter/map
- [x] `ctx.watch_channel::<T>()` returns `(Sender<T>, Watch<T>)`
- [x] `glob_filter` is a public utility function
- [x] `watch` attribute removed from `#[runme::task]` macro and `TaskDef`
- [x] All existing tests pass; new tests cover new types
- [x] `cargo build` and `cargo test` pass

## Human Review Gates

1. **After Phase 1 (Output)** — Verify API shape before TUI/Watch build on it. `Human Review: true, Auto-Approve: false`. Rationale: Output type is load-bearing for everything else; wrong shape here cascades.
2. **After Phase 2+3 merge** — Verify TUI and Watch work together. `Auto-Approve: true`. Rationale: acceptance criteria are testable; if tests pass, merge is safe.

---

## Status

`complete` — All phases implemented and validated.

## Context

### Key Files

| File | Role | Modified By |
|------|------|-------------|
| `crates/runme/src/process.rs` | ExecOutput, ProcessHandle, exec/spawn fns | Phase 1 |
| `crates/runme/src/task.rs` | TaskContext, TaskDef, Registry | Phase 1, 2, 3 |
| `crates/runme/src/error.rs` | ProcessError → TaskError conversion | Phase 1 |
| `crates/runme/src/prelude.rs` | Public re-exports | Phase 1, 2, 3 |
| `crates/runme/src/tui/app.rs` | TUI state machine, event loop, terminal mgmt | Phase 2 |
| `crates/runme/src/tui/runner.rs` | TaskRunner, ProcessInfo, spawn monitoring | Phase 2 |
| `crates/runme/src/tui/event.rs` | Event loop (run_event_loop) | Phase 2 |
| `crates/runme/src/log/buffer.rs` | OutputBuffer | Phase 1 (minor) |
| `crates/runme-macros/src/lib.rs` | #[runme::task] macro | Phase 3 |
| `crates/runme/Cargo.toml` | Dependencies | Phase 3 (notify, globset) |

### Current State

- `ExecOutput { stdout: String, stderr: String }` — used by exec() return and ProcessError::ExitCode
- `ProcessHandle.buffer` is public `Arc<Mutex<OutputBuffer>>` — no `.output()` method
- `ProcessHandle.wait()` returns empty ExecOutput (output is in buffer) — semantic mismatch
- TUI always uses alternate screen, never auto-exits, writes nothing to stdio after close
- No file watching infrastructure; `notify` crate not in deps
- Macro parses `watch` attribute into `TaskDef.watch` field but nothing reads it at runtime

---

## Team

| Name | Role | Agent Type | Model | Strategy |
|------|------|-----------|-------|----------|
| `output-impl` | Implement Output type, ProcessResult, migrate from ExecOutput | general-purpose | opus | subagent |
| `output-validator` | Validate Phase 1: build, test, API review | general-purpose | sonnet | subagent |
| `tui-impl` | Implement tui_wait, TuiOutput, event loop hooks | general-purpose | opus | subagent |
| `tui-validator` | Validate Phase 2: build, test, manual TUI check | general-purpose | sonnet | subagent |
| `watch-impl` | Implement Watch<T>, ctx.watch variants, glob_filter, macro cleanup | general-purpose | opus | subagent |
| `watch-validator` | Validate Phase 3: build, test, watch behavior | general-purpose | sonnet | subagent |
| `integration-validator` | Final validation after merge | general-purpose | sonnet | subagent |

---

## Phase 1: Output Unification

### Task: `output-types`

- **Assigned To:** `output-impl`
- **Depends On:** none
- **Parallel:** no (foundational)
- **Human Review:** false
- **Description:**

Create the `Output` type and `ProcessResult` type. Update `exec()` and `ProcessHandle` to use them. Remove `ExecOutput`.

**Step-by-step:**

1. **Create `Output` type** in `process.rs`:
   ```rust
   pub struct Output(Arc<Mutex<OutputBuffer>>);
   impl Output {
       pub fn entries(&self) -> Vec<LogEntry> { /* lock, clone lines */ }
       pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> { /* lock, subscribe */ }
       pub fn stdout(&self) -> Vec<String> { /* filter entries by stream */ }
       pub fn stderr(&self) -> Vec<String> { /* filter entries by stream */ }
   }
   ```
   Note: `LogEntry` currently has no stdout/stderr stream distinction. Check how process.rs tags entries from stdout vs stderr readers — may need to add a `stream` field to `LogEntry` or use the `source` field convention. Investigate before implementing.

2. **Create `ProcessResult` type** in `process.rs`:
   ```rust
   pub struct ProcessResult {
       exit_code: i32,
       output: Output,
   }
   impl ProcessResult {
       pub fn success(&self) -> bool { self.exit_code == 0 }
       pub fn exit_code(&self) -> i32 { self.exit_code }
       pub fn output(&self) -> &Output { &self.output }
       pub fn ok(self) -> Result<ProcessResult, ProcessResult> {
           if self.success() { Ok(self) } else { Err(self) }
       }
   }
   ```
   Implement `From<ProcessResult> for TaskError` so `?` works on `Result<ProcessResult, ProcessResult>`.

3. **Update `process::exec()`** to return `ProcessResult` instead of `Result<ExecOutput, ProcessError>`:
   - The function currently creates an `OutputBuffer`, runs the process, captures into both ExecOutput strings and the buffer. Remove the string capture — output lives only in the buffer.
   - Return `ProcessResult { exit_code, output: Output(buffer_arc) }` always (success and failure).
   - `ProcessError` still exists for spawn failures (Spawn, Signal, Wait, Timeout) but `ExitCode` variant is removed — non-zero exit is expressed via `ProcessResult.success() == false`.

4. **Update `TaskContext::exec()`** to return `ProcessResult`.

5. **Update `ProcessHandle`**:
   - Replace public `buffer: Arc<Mutex<OutputBuffer>>` field with private field + `.output() -> &Output` method.
   - Update `ProcessHandle::wait()` to return `ProcessResult` instead of `Result<ExecOutput, ProcessError>`.

6. **Update `ProcessError`**: Remove the `ExitCode` variant. Non-zero exit is no longer an error — it's a `ProcessResult` with `success() == false`. Keep `Spawn`, `Signal`, `Wait`, `Timeout` variants.

7. **Update `error.rs`**: The `From<ProcessError> for TaskError` conversion no longer needs to handle `ExitCode`. Add `From<ProcessResult> for TaskError` that extracts exit code and stderr from the Output.

8. **Remove `ExecOutput`** and `ExecOutputExt` trait entirely.

9. **Update `prelude.rs`**: Export `Output`, `ProcessResult`. Remove `ExecOutput`, `ExecOutputExt`.

10. **Update TUI runner.rs**: `SpawnEvent` and `ProcessInfo` currently use `buffer: Arc<Mutex<OutputBuffer>>`. These should use `Output` or continue using the arc directly (Output wraps it). The `monitor_spawns` function subscribes to the buffer — this should work through `Output.subscribe()`.

11. **Update all tests** in `process.rs` and `task.rs`:
    - Tests that check `output.stdout` / `output.stderr` strings → use `result.output().stdout()` / `.stderr()`
    - Tests that check `ProcessError::ExitCode { code, .. }` → check `result.success()` and `result.exit_code()`
    - Tests that access `handle.buffer` directly → use `handle.output()`

**Reference:** `docs/system_design.md` sections "Process Output" and "Execution Results".

### Task: `output-validate`

- **Assigned To:** `output-validator`
- **Depends On:** `output-types`
- **Parallel:** no
- **Human Review:** true (Phase 1 gate — verify API shape)
- **Description:**

Validate the Output unification:
1. `cargo build` succeeds
2. `cargo test` — all tests pass
3. Verify `ExecOutput` is fully removed (grep for it)
4. Verify `Output` type has the documented API: `.entries()`, `.subscribe()`, `.stdout()`, `.stderr()`
5. Verify `ProcessResult` has `.ok()`, `.success()`, `.exit_code()`, `.output()`
6. Verify `ProcessError` no longer has `ExitCode` variant
7. Check that `From<ProcessResult> for TaskError` exists and is reasonable
8. Run `cargo doc` on the runme crate and spot-check the Output/ProcessResult docs

---

## Phase 2: TUI Behavior Control

### Task: `tui-behavior`

- **Assigned To:** `tui-impl`
- **Depends On:** `output-validate`
- **Parallel:** yes (with `watch-core`)
- **Human Review:** false
- **Description:**

Add `tui_wait` and `tui_output` primitives to TaskContext and wire them through the TUI.

**Step-by-step:**

1. **Add state to `TaskContext`** in `task.rs`:
   ```rust
   // New fields
   tui_wait: Arc<AtomicBool>,      // default: true
   tui_output: Arc<Mutex<TuiOutput>>,
   ```
   Add methods:
   - `ctx.tui_wait(wait: bool)` — sets the atomic
   - `ctx.tui_output() -> TuiOutputHandle` — returns a handle for building up the output
   - `ctx.task_output() -> Output` — returns Output wrapping the task's tracing OutputBuffer

2. **Create `TuiOutput` type** (new file `crates/runme/src/tui/output.rs` or in `task.rs`):
   ```rust
   pub struct TuiOutput {
       stdout_entries: Vec<LogEntry>,
       stderr_entries: Vec<LogEntry>,
       subscriptions: Vec<(Stream, broadcast::Receiver<LogEntry>)>,
   }
   ```
   Where `Stream` is `Stdout | Stderr | Preserve`.

   `TuiOutputHandle` (or use `&TuiOutput` with interior mutability) provides:
   - `.append(output: &Output)` — copy current entries, preserving stdout/stderr mapping
   - `.subscribe(output: &Output)` — start following live entries, preserving mapping
   - `.stdout()` → returns a stream-targeted builder with its own `.append()` / `.subscribe()`
   - `.stderr()` → same, targeting stderr
   - `.write(text: &str)` — literal text to the targeted stream

   Internal: `flush(&mut self) -> (Vec<u8>, Vec<u8>)` — drains subscriptions, combines with appended entries, returns final stdout/stderr bytes.

3. **Wire `tui_wait` into the event loop** (`tui/event.rs` or `tui/app.rs`):
   - The event loop currently runs `while state.running`. Add a check: when task status becomes `Done` or `Failed`, if `tui_wait` is false (for Done) or still false (for Failed), set `state.running = false`.
   - The `tui_wait` Arc<AtomicBool> needs to be passed from TaskContext through TaskRunner to the event loop. TaskRunner already shares `task_status: Arc<Mutex<TaskStatus>>` — add `tui_wait` similarly.

4. **Wire `tui_output` flush into TUI shutdown** (`tui/app.rs`):
   - After `restore_terminal()` (which exits alternate screen and restores normal terminal), flush the `TuiOutput` buffer to real stdout/stderr.
   - The `TuiOutput` Arc<Mutex<>> needs to be accessible from App. Pass it through TaskRunner like other shared state.
   - Drain any active subscriptions before flushing.

5. **Non-TUI modes**: `tui_wait()` and `tui_output()` calls should be no-ops. Since there's currently no non-TUI mode, this is future-proofing. The simplest approach: TaskContext always has the fields, but if nobody reads `tui_output`, nothing happens. `tui_wait` defaults to true, which is correct for both TUI (stay open) and future CLI mode (wait for completion anyway).

6. **Add tests**:
   - Unit test: `TuiOutput` append copies entries correctly
   - Unit test: `TuiOutput` subscribe captures live entries
   - Unit test: `TuiOutput` stdout/stderr targeting works
   - Unit test: `tui_wait` flag is readable from outside TaskContext

**Reference:** `docs/system_design.md` section "TUI Behavior Control".

### Task: `tui-validate`

- **Assigned To:** `tui-validator`
- **Depends On:** `tui-behavior`
- **Parallel:** no
- **Human Review:** false
- **Description:**

Validate TUI behavior control:
1. `cargo build` succeeds
2. `cargo test` — all tests pass
3. Verify `ctx.tui_wait(bool)` exists and is wired to event loop
4. Verify `ctx.tui_output()` returns a handle with `.append()`, `.subscribe()`, `.stdout()`, `.stderr()`, `.write()`
5. Verify `ctx.task_output()` returns `Output`
6. Grep for `TuiOutput` to confirm type exists with expected methods
7. Check that `restore_terminal()` path includes output flush

---

## Phase 3: Watch API

### Task: `watch-core`

- **Assigned To:** `watch-impl`
- **Depends On:** `output-validate`
- **Parallel:** yes (with `tui-behavior`)
- **Human Review:** false
- **Description:**

Implement the Watch API: `Watch<T>` type, three constructors on TaskContext, `glob_filter` utility, and macro cleanup.

**Step-by-step:**

1. **Add dependencies** to `crates/runme/Cargo.toml`:
   - `notify = "6"` — filesystem event watching
   - `globset = "0.4"` — efficient glob pattern matching

2. **Create `Watch<T>` type** (new file `crates/runme/src/watch.rs`):
   ```rust
   pub struct Watch<T> {
       rx: mpsc::UnboundedReceiver<T>,
       label: Option<String>,
       // For TUI visibility:
       watch_info: Arc<Mutex<WatchInfo>>,
   }

   struct WatchInfo {
       label: Option<String>,
       kind: WatchKind,  // FileGlob(String) | Custom | Channel
       trigger_count: u64,
       last_triggered: Option<Instant>,
   }

   impl<T> Watch<T> {
       pub async fn next(&mut self) -> T { self.rx.recv().await.unwrap() }
       pub fn label(mut self, label: &str) -> Self { ... }
   }
   ```

3. **Create `glob_filter` utility** in `watch.rs`:
   ```rust
   pub fn glob_filter(pattern: &str, paths: &[PathBuf]) -> Vec<PathBuf> {
       // Use globset to compile pattern, filter paths
   }
   ```

4. **Add watch constructors to `TaskContext`** in `task.rs`:

   `ctx.watch("glob")` → `Watch<Vec<PathBuf>>`:
   - Create a `notify::RecommendedWatcher` watching the RUNME.rs file's directory (or cwd)
   - Debounce events (notify v6 has built-in debouncer, or use a short timer)
   - Filter through glob pattern
   - Send matching paths through mpsc channel
   - Return `Watch<Vec<PathBuf>>`

   `ctx.watch_with(f)` → `Watch<T>` where `F: Fn(&[PathBuf]) -> Option<T>`:
   - Same filesystem watcher setup
   - Run all changed paths through the closure
   - If `Some(t)`, send through channel
   - If `None`, discard (keep waiting)

   `ctx.watch_channel::<T>()` → `(mpsc::UnboundedSender<T>, Watch<T>)`:
   - Create an mpsc channel pair
   - Wrap receiver in `Watch<T>`
   - Return sender and watch

   All constructors register the watch with TaskContext for TUI visibility (store `WatchInfo` Arc in a list on TaskContext).

5. **Remove `watch` from macro** in `crates/runme-macros/src/lib.rs`:
   - Remove parsing of `watch = "..."` attribute
   - Remove watch token generation
   - Update any error messages

6. **Remove `watch` field from `TaskDef`** in `task.rs`:
   - Remove `watch: Option<&'static str>` field
   - Update `inventory` registration in macro output

7. **Wire watch visibility to TUI** (minimal):
   - Add `watches: Arc<Mutex<Vec<Arc<Mutex<WatchInfo>>>>>` to TaskContext
   - TaskRunner can expose this for the sidebar to display watch status
   - This is display-only for now — just show label + trigger count

8. **Add tests**:
   - Unit test: `glob_filter` matches correctly
   - Unit test: `watch_channel` sends and receives
   - Unit test: `Watch::label()` sets the label
   - Integration test: `ctx.watch()` detects file changes (create temp dir, write file, verify `.next()` returns)
   - Integration test: `ctx.watch_with()` filters correctly

**Reference:** `docs/system_design.md` section "Watch API".

### Task: `watch-validate`

- **Assigned To:** `watch-validator`
- **Depends On:** `watch-core`
- **Parallel:** no
- **Human Review:** false
- **Description:**

Validate Watch API:
1. `cargo build` succeeds
2. `cargo test` — all tests pass
3. Verify `ctx.watch()`, `ctx.watch_with()`, `ctx.watch_channel()` exist with correct signatures
4. Verify `Watch<T>` has `.next()` and `.label()`
5. Verify `glob_filter` is public
6. Verify `watch` attribute is removed from macro (grep `crates/runme-macros/` for "watch")
7. Verify `TaskDef` no longer has `watch` field
8. Verify `notify` and `globset` are in Cargo.toml

---

## Phase 4: Integration

### Task: `integration-check`

- **Assigned To:** `integration-validator`
- **Depends On:** `tui-validate`, `watch-validate`
- **Parallel:** no
- **Human Review:** false (auto-approve — if tests pass, it's good)
- **Description:**

Final validation after TUI and Watch branches are merged:
1. `cargo build` succeeds with no warnings
2. `cargo test` — all tests pass
3. `cargo clippy` — no new warnings
4. Verify the example from the design doc compiles conceptually (the "install" task example)
5. Verify no references to `ExecOutput` remain anywhere in the codebase
6. Verify design doc examples are consistent with the implemented API

---

## Validation Profile

```yaml
validation:
  build:
    command: "cargo build"
    required: true
  tests:
    command: "cargo test"
    required: true
  clippy:
    command: "cargo clippy -- -D warnings"
    required: false
  doc:
    command: "cargo doc --no-deps -p runme"
    required: false
```

## Findings

- **Phase 1**: `LogEntry` had no stdout/stderr distinction. Added `Stream` enum and `stream: Option<Stream>` field. Output methods are async because `OutputBuffer` uses `tokio::sync::Mutex`. `exec()` returns `Result<ProcessResult, ProcessError>` — ProcessError for spawn failures, ProcessResult for all exit codes. 483 tests pass.

## Decisions Log

(populated during execution)

## Blockers

(populated during execution)
