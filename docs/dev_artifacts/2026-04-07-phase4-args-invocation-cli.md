# Phase 4: Task Arguments, Invocation & CLI

## Goal

Implement the unified invocation model: task arguments via clap, cross-file task invocation (`ctx.run()`), task/step observation, TUI multi-task support, and the full CLI dispatch with built-in commands.

## Approach

Build bottom-up: core types first (TaskDef changes, observation guards), then macro argument support and invocation APIs in parallel, then TUI multi-task and CLI dispatch in parallel once their dependencies land.

Critical path: **core-types → macro-args → cli-dispatch**. TUI work runs on a parallel track: **core-types → core-invocation → tui-multitask**.

## Acceptance Criteria

- [ ] `#[task]` macro supports three signature forms: zero args, simple params, clap::Parser struct
- [ ] Task argument metadata is extractable at runtime (flag names, types, help text)
- [ ] `ctx.run("group:task", &["--flag", "val"])` invokes cross-file tasks through the registry
- [ ] `ctx.tasks()` queries the registry with glob matching
- [ ] `ctx.start_task()` and `ctx.begin_step()` RAII guards notify the runtime on enter/exit
- [ ] TUI sidebar displays task tree with nested processes and steps
- [ ] Multiple concurrent tasks render correctly in TUI
- [ ] `RunmeArgs` parses `--ui tui|cli|agent` and forwards rest to task parser
- [ ] Built-in `list` task works as `runme list` and `runme :list`
- [ ] `:` prefix resolves to `builtin:` group
- [ ] Generated runner main is slim: link, init, call `runme::cli::run()`
- [ ] `cargo build` succeeds, `cargo test` passes, `cargo clippy` clean
- [ ] Example RUNME.rs demonstrates cross-file invocation with arguments

## Human Review Gates

1. **After Phase 1 (Core Foundation)** — The type changes to TaskDef and the observation/invocation APIs define the shape everything else builds on.
   - Review Rationale: Data model changes that are hard to reverse. All downstream work depends on these types.
   - Human Review: true, Auto-Approve: false

2. **Final review after Phase 4** — End-to-end validation.
   - Review Rationale: Verifying the full integration works as designed.
   - Human Review: true, Auto-Approve: false

---

## Status

implementation — Phases 1-3 complete, Phase 4 (examples) pending

## Context

### Key Files

| File | Role | Modified By |
|------|------|-------------|
| `crates/runme/src/task.rs` | TaskDef, TaskContext, Registry | core-types, core-invocation |
| `crates/runme-macros/src/lib.rs` | `#[task]` macro expansion | macro-args |
| `crates/runme/src/tui/runner.rs` | TaskRunner lifecycle | tui-multitask |
| `crates/runme/src/tui/sidebar.rs` | Sidebar entries/rendering | tui-multitask |
| `crates/runme/src/tui/app.rs` | App state machine | tui-multitask |
| `crates/runme/src/tui/event.rs` | Event loop, status polling | tui-multitask |
| `crates/runme-cli/src/codegen.rs` | Generated runner main | cli-dispatch |
| `crates/runme/src/cli.rs` | NEW: RunmeArgs, cli::run() | cli-dispatch |
| `crates/runme/src/builtin.rs` | NEW: built-in tasks (list, completions) | cli-dispatch |

### Current State

- `TaskDef` has: name, description, group, func (TaskFn)
- `TaskFn` is `fn(&TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>>`
- `#[task]` macro handles 4 variants (async/sync x with/without return), all taking only `&TaskContext`
- `Registry::from_inventory()` collects tasks, `run()` creates a fresh TaskContext per invocation
- TUI is single-task: one TaskRunner, one task_status, flat process list in sidebar
- Generated runner main has inline dispatch logic (~80 lines of generated Rust)

### Design References

- `docs/system_design.md` § Task Arguments, § CLI Argument Model, § Built-in Commands, § Task Lifecycle & Runtime Visibility
- `docs/01-implementation-plan.md` § Phase 4, § Phase 4b

---

## Team

| Name | Role | Agent Type | Model | Strategy |
|------|------|-----------|-------|----------|
| core-types | Implementor | general-purpose | opus | subagent |
| core-invocation | Implementor | general-purpose | opus | subagent |
| macro-args | Implementor | general-purpose | opus | subagent |
| tui-multitask | Implementor | general-purpose | opus | subagent |
| cli-dispatch | Implementor | general-purpose | opus | subagent |
| phase-validator | Validator | general-purpose | sonnet | subagent |

---

## Phase 1: Core Foundation

### Task: core-types

- **ID:** core-types
- **Depends On:** none
- **Assigned To:** core-types
- **Parallel:** no (first task)
- **Plan Approval:** true
- **Human Review:** true (after completion, before Phase 2 starts)

**Description:**

Update the core type system in `crates/runme/src/task.rs` to support argument metadata, task observation, and step tracking.

**Changes to TaskDef:**

Add an `arg_metadata` field to `TaskDef`. This is a function pointer that returns argument info at runtime:

```rust
pub type ArgMetadataFn = fn() -> Option<clap::Command>;

pub struct TaskDef {
    pub name: &'static str,
    pub description: Option<&'static str>,
    pub group: &'static str,
    pub func: TaskFnKind,             // Static(TaskFn) or Dynamic(Arc<dyn Fn>)
    pub arg_metadata: ArgMetadataFn,  // NEW
    pub ui_hint: Option<UiHint>,
}
```

For tasks with a Parser param, the macro will generate `|| Some(<ParserType>::command())`. For zero-arg and simple-arg tasks, the macro will generate the Command from the param metadata. The `arg_metadata` function is called at runtime for discovery (e.g., `runme :list` showing available flags).

Also update `TaskFn` to accept args:

```rust
pub type TaskFn = fn(&TaskContext, &[String]) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>>;
```

The wrapper generated by `#[task]` will parse the `&[String]` into the appropriate types. For zero-arg tasks, the wrapper ignores the slice. This is the uniform invocation interface — CLI, ctx.run(), and MCP all pass string args.

**Observation Guards:**

Add `TaskGuard` and `StepGuard` types. These are RAII guards that notify the runtime on creation and drop. For now, they emit tracing events (we can change the notification mechanism later, but tracing gets us visibility in the log viewer immediately):

```rust
pub struct TaskGuard { name: String }
pub struct StepGuard { name: String, failed: bool }

impl TaskContext {
    pub fn start_task(&self, name: &str) -> TaskGuard { ... }
    pub fn begin_step(&self, name: &str) -> StepGuard { ... }
}

impl StepGuard {
    pub fn fail(&mut self, reason: &str) { ... }
}

impl Drop for TaskGuard { ... }  // emits task exit event
impl Drop for StepGuard { ... }  // emits step exit event (with failure if fail() was called)
```

**TaskInfo type for discovery:**

Add a type that `ctx.tasks()` will return:

```rust
pub struct TaskInfo {
    pub name: &'static str,
    pub group: &'static str,
    pub description: Option<&'static str>,
    pub qualified_name: String,  // "group:name" or just "name" for root
}
```

**Read before editing:** `crates/runme/src/task.rs`, `crates/runme/src/lib.rs`, `crates/runme/src/prelude.rs`

**Validation:** `cargo build` succeeds across the workspace. Existing tests still pass. New types are exported from prelude where appropriate.

---

### Task: core-invocation

- **ID:** core-invocation
- **Depends On:** core-types
- **Assigned To:** core-invocation
- **Parallel:** yes (parallel with macro-args, different files)
- **Plan Approval:** true

**Description:**

Add cross-file invocation and task discovery APIs to TaskContext in `crates/runme/src/task.rs`.

**Registry sharing:**

Change `Registry` to be shareable via `Arc`:

```rust
impl TaskContext {
    // Add registry field
    registry: Option<Arc<Registry>>,

    pub fn set_registry(&mut self, registry: Arc<Registry>) { ... }
}
```

Update `Registry::run()` to accept args and create a TaskContext with the registry injected:

```rust
impl Registry {
    pub async fn run(&self, name: &str, args: &[String], self_arc: &Arc<Registry>) -> Result<(), TaskError> {
        let task = self.resolve(name)?;
        let mut ctx = TaskContext::new(task.name);
        ctx.set_registry(self_arc.clone());
        (task.func)(&ctx, args).await
    }
}
```

**ctx.run() and ctx.tasks():**

```rust
impl TaskContext {
    /// Invoke a task by name with string arguments. Blocks until the task returns.
    pub async fn run(&self, name: &str, args: &[&str]) -> TaskResult {
        let registry = self.registry.as_ref()
            .ok_or_else(|| TaskError::from_display("no registry available"))?;
        let string_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        registry.run(name, &string_args, registry).await
    }

    /// Query the task registry.
    pub fn tasks(&self) -> TaskQuery { ... }
}

pub struct TaskQuery { ... }

impl TaskQuery {
    pub fn matching(&self, pattern: &str) -> Vec<TaskInfo> { ... }  // glob match on "group:name"
    pub fn all(&self) -> Vec<TaskInfo> { ... }
}
```

**Task resolution refactor:**

Move the 3-tier task resolution logic (currently inline in codegen.rs) into `Registry::resolve()` so it can be reused by ctx.run(), CLI dispatch, and the generated runner. Handle the `:` alias for `builtin:` here too.

```rust
impl Registry {
    pub fn resolve(&self, name: &str) -> Result<&'static TaskDef, TaskError> {
        // Handle : prefix → builtin:
        // Handle group:task explicit
        // Handle short name with root-wins disambiguation
    }
}
```

**Read before editing:** `crates/runme/src/task.rs`, `crates/runme-cli/src/codegen.rs` (for the resolution logic to move)

**Validation:** `cargo build` succeeds. Existing tests pass. Write unit tests for `Registry::resolve()` covering: exact match, group:task, ambiguous with root-wins, `:` alias, unknown task.

---

### Task: validate-phase1

- **ID:** validate-phase1
- **Depends On:** core-types, core-invocation
- **Assigned To:** phase-validator
- **Parallel:** no
- **Human Review:** true

**Description:**

Run full validation after Phase 1:

```bash
cargo build --workspace 2>&1
cargo test --workspace 2>&1
cargo clippy --workspace 2>&1
```

Check that:
- TaskDef has arg_metadata and updated TaskFn signature
- TaskGuard and StepGuard exist with RAII semantics
- ctx.start_task(), ctx.begin_step(), ctx.run(), ctx.tasks() exist on TaskContext
- Registry::resolve() handles all disambiguation cases
- All existing tests pass (macro tests may need TaskFn signature updates)

Report results. Surface any failures for human review.

---

## Phase 2: Macro & TUI (parallel)

### Task: macro-args

- **ID:** macro-args
- **Depends On:** core-types
- **Assigned To:** macro-args
- **Parallel:** yes (parallel with core-invocation and tui-multitask)
- **Plan Approval:** true

**Description:**

Update the `#[task]` macro in `crates/runme-macros/src/lib.rs` to support the three argument forms and inject `start_task()`.

**Signature detection:**

The macro inspects the function parameters (after `ctx: &TaskContext`):

1. **Zero args:** `async fn build(ctx: &TaskContext) -> TaskResult`
   - Wrapper ignores the `&[String]` arg slice
   - `arg_metadata` returns `None`

2. **Simple args:** `async fn deploy(ctx: &TaskContext, env: String, port: u16, verbose: bool) -> TaskResult`
   - Wrapper parses `&[String]` using a generated clap::Command (one `--flag` per param)
   - Type mapping: `String`/numeric → `--name <value>`, `bool` → `--name` (flag), `Option<T>` → optional, `Vec<T>` → repeatable
   - `arg_metadata` returns the same generated Command

3. **Parser struct:** `async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult` where the second param is a single non-primitive type
   - Wrapper calls `DeployArgs::try_parse_from(args)` on the `&[String]` slice
   - `arg_metadata` returns `Some(DeployArgs::command())`

**Detection heuristic:** After filtering out `ctx`, count remaining params. Zero → form 1. Multiple → form 2. One param whose type is not a known primitive (`String`, `bool`, numeric types, `Option<T>`, `Vec<T>`) → form 3.

**start_task() injection:**

The macro injects `let _task = ctx.start_task("task_name");` as the first statement in the function body.

**Updated wrapper generation:**

Currently the wrapper signature is:
```rust
fn wrapper(ctx: &TaskContext) -> Pin<Box<...>>
```

Change to:
```rust
fn wrapper(ctx: &TaskContext, __args: &[String]) -> Pin<Box<...>>
```

For form 1, ignore `__args`. For form 2, parse with generated clap. For form 3, parse with `Parser::try_parse_from`.

**Read before editing:** `crates/runme-macros/src/lib.rs`, `crates/runme/src/task.rs` (for the new TaskFn type)

**Validation:** `cargo build --workspace`. Write tests in the macro crate for each of the three forms. Test that arg parsing errors produce good error messages.

---

### Task: tui-multitask

- **ID:** tui-multitask
- **Depends On:** core-invocation
- **Assigned To:** tui-multitask
- **Parallel:** yes (parallel with macro-args and cli-dispatch)
- **Plan Approval:** true

**Description:**

Refactor the TUI to support multiple concurrent tasks. Currently the TUI tracks one TaskRunner with one task_status and one flat process list. After this change, multiple tasks can run simultaneously with their processes and steps nested in the sidebar.

**TaskRunner changes (`tui/runner.rs`):**

- Allow `launch()` to be called multiple times (don't consume `spawn_rx` — use a shared channel or create per-task channels)
- Each launched task gets a unique ID and its own status, process list, and tracing buffer
- Introduce a `TaskSession` concept:

```rust
struct TaskSession {
    id: usize,
    task_name: String,
    status: Arc<Mutex<TaskStatus>>,
    processes: Arc<Mutex<Vec<ProcessInfo>>>,
}
```

- TaskRunner holds `sessions: Vec<TaskSession>` instead of single status/processes
- All sessions share the same `LogStore` (output from all tasks interleaves)
- The spawn_tx/spawn_rx channel stays shared — `SpawnEvent` already carries `task_name` so the monitor can associate processes with the right session

**Sidebar changes (`tui/sidebar.rs`):**

- `build_sidebar_entries()` now groups entries by task session:
  - Task entry (top level per session)
  - Step entries nested under task (when step observation is wired)
  - Process entries nested under task
- Visual hierarchy: task name is bold/highlighted, processes indented under it
- Selection works across the tree: navigate between tasks and their children

**App state changes (`tui/app.rs`):**

- Replace `task_status: Option<Arc<Mutex<TaskStatus>>>` with access through TaskRunner sessions
- Replace `task_name: Option<String>` — derive from sessions
- Replace `processes: Option<Arc<Mutex<Vec<ProcessInfo>>>>` — derive from sessions
- Source filtering still works per-source (each process/task has a source string)
- When `ctx.run()` is called from a task, the runtime should launch it as a new session in the existing TaskRunner

**Event loop changes (`tui/event.rs`):**

- `refresh_sidebar_state()` iterates all sessions, not just one
- Process status polling covers all sessions

**Read before editing:** `crates/runme/src/tui/runner.rs`, `crates/runme/src/tui/sidebar.rs`, `crates/runme/src/tui/app.rs`, `crates/runme/src/tui/event.rs`, `crates/runme/src/tui/render.rs`

**Validation:** `cargo build`. TUI launches and displays tasks. Existing single-task usage still works. If possible, test with a RUNME.rs that spawns multiple processes to verify sidebar grouping.

---

### Task: validate-phase2

- **ID:** validate-phase2
- **Depends On:** macro-args, tui-multitask
- **Assigned To:** phase-validator
- **Parallel:** no

**Description:**

Run full validation after Phase 2:

```bash
cargo build --workspace 2>&1
cargo test --workspace 2>&1
cargo clippy --workspace 2>&1
```

Check that:
- `#[task]` macro compiles with all three argument forms
- TUI builds and the sidebar data model supports multiple sessions
- No regressions in existing functionality

Report results.

---

## Phase 3: CLI Dispatch

### Task: cli-dispatch

- **ID:** cli-dispatch
- **Depends On:** core-invocation, macro-args
- **Assigned To:** cli-dispatch
- **Parallel:** yes (parallel with tui-multitask if it's still running)
- **Plan Approval:** true

**Description:**

Implement the two-stage CLI argument model, built-in commands, and slim down the generated runner.

**RunmeArgs (`crates/runme/src/cli.rs` — new file):**

```rust
use clap::Parser;

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum UiMode { Tui, Cli, Agent }

#[derive(Parser)]
#[command(name = "runme")]
pub struct RunmeArgs {
    /// UI mode
    #[arg(long, default_value = "tui")]
    pub ui: UiMode,

    /// Output format
    #[arg(long, default_value = "text")]
    pub format: OutputFormat,

    /// Timeout (seconds, or with suffix: 10m, 1h)
    #[arg(long)]
    pub timeout: Option<String>,

    /// Log filter expression
    #[arg(long)]
    pub filter: Option<String>,

    /// Task name and task-specific arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}
```

**cli::run() function:**

```rust
pub async fn run(registry: Arc<Registry>, group_names: HashMap<String, String>) {
    let args = RunmeArgs::parse();

    if args.rest.is_empty() {
        // No task specified — TUI picker (or error in cli/agent mode)
        match args.ui {
            UiMode::Tui => { /* launch picker */ }
            _ => { eprintln!("No task specified"); std::process::exit(1); }
        }
        return;
    }

    let task_name = &args.rest[0];
    let task_args = &args.rest[1..];

    let task = registry.resolve(task_name)?;

    match args.ui {
        UiMode::Tui => { /* App::with_task, pass task_args */ }
        UiMode::Cli => { /* direct execution, stdio output */ }
        UiMode::Agent => { /* structured output, minimal */ }
    }
}
```

**Built-in tasks (`crates/runme/src/builtin.rs` — new file):**

Define built-in tasks using `#[task]` with group set to `"builtin"`. These are registered via inventory like any other task.

```rust
const __RUNME_GROUP: &str = "builtin";

/// List available tasks
#[runme::task]
async fn list(ctx: &TaskContext) -> TaskResult {
    for task in ctx.tasks().all() {
        // Print task name, group, description, available args
    }
    Ok(())
}

/// Generate shell completions
#[runme::task]
async fn completions(ctx: &TaskContext, shell: String) -> TaskResult {
    // Generate completions for the given shell
    Ok(())
}
```

**Slim codegen (`crates/runme-cli/src/codegen.rs`):**

Replace the ~80 lines of inline dispatch with:

```rust
fn main() {
    // __runme_link() calls (same as now)

    runme::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(async {
            // Init hooks (same as now — leaf-to-root)
            // Build group_names (same as now)
            let registry = Arc::new(runme::task::Registry::from_inventory());
            runme::cli::run(registry, group_names).await;
        });
}
```

The generated code does only: link, init, build registry, hand off. All dispatch logic lives in the library.

**Read before editing:** `crates/runme-cli/src/codegen.rs`, `crates/runme/src/lib.rs`, `crates/runme/src/task.rs` (Registry), `crates/runme/src/tui/app.rs` (App::with_task signature may need task_args)

**Validation:** `cargo build --workspace`. Test `runme --list` (old flag) still works or is replaced by `runme :list`. Test `runme --ui cli <task>` runs without TUI.

---

### Task: validate-phase3

- **ID:** validate-phase3
- **Depends On:** cli-dispatch, validate-phase2
- **Assigned To:** phase-validator
- **Parallel:** no

**Description:**

Run full validation after Phase 3:

```bash
cargo build --workspace 2>&1
cargo test --workspace 2>&1
cargo clippy --workspace 2>&1
```

Check that:
- `runme :list` works (or the compiled binary equivalent)
- `RunmeArgs` parses correctly
- Generated runner main is slim
- Built-in tasks are registered and resolvable
- `:` prefix resolves correctly

Report results.

---

## Phase 4: Integration & Examples

### Task: integration-example

- **ID:** integration-example
- **Depends On:** validate-phase3
- **Assigned To:** cli-dispatch
- **Parallel:** no

**Description:**

Create or update example RUNME.rs files demonstrating the new features:

1. **Task with simple args:** `async fn deploy(ctx: &TaskContext, env: String, port: u16)`
2. **Task with Parser struct:** `async fn deploy(ctx: &TaskContext, args: DeployArgs)`
3. **Cross-file invocation:** Root RUNME.rs with `test_all` that discovers and runs `*:test` tasks
4. **Steps:** A task using `ctx.begin_step()` for labeled phases

Update `examples/RUNME.rs` or add new examples under `examples/`.

**Validation:** The examples compile and run via the `runme` binary.

---

### Task: final-validation

- **ID:** final-validation
- **Depends On:** integration-example
- **Assigned To:** phase-validator
- **Parallel:** no
- **Human Review:** true

**Description:**

End-to-end validation:

```bash
cargo build --workspace 2>&1
cargo test --workspace 2>&1
cargo clippy --workspace 2>&1
```

Then test the full pipeline:
1. Build with `cargo build -p runme-cli`
2. Run the compiled binary against the examples
3. Verify `runme :list` shows built-in and example tasks
4. Verify task with args works from CLI
5. Verify TUI launches and shows task tree in sidebar

Report comprehensive results for human review.

---

## Validation Profile

```yaml
validation:
  build:
    command: "cargo build --workspace"
    required: true
  tests:
    command: "cargo test --workspace"
    required: true
  lint:
    command: "cargo clippy --workspace"
    required: true
```

## Dependency Graph

```
core-types ──→ core-invocation ──→ tui-multitask ──→ validate-phase2
           └─→ macro-args ──────────────────────────→ validate-phase2
                             └─→ cli-dispatch ──→ validate-phase3
               core-invocation ──→ cli-dispatch
                                                    validate-phase3 → integration-example → final-validation
```

## Findings

- **Macro `ctx` hardcoding**: The `#[task]` macro injected `ctx.start_task(...)` with a hardcoded `ctx` identifier. Functions using `_ctx` or other names broke. Fixed by extracting the actual first parameter name.
- **TUI exit regression**: The multi-session refactor in `launch_picked_task` captured `runner.status` before `launch()`, getting a stale Arc that never transitioned to Done. Fixed by capturing after launch.
- **CLI mode missing output**: `run_cli` created a bare TaskContext with no tracing subscriber and no output forwarder. Added `tracing_subscriber::fmt` for stderr and a broadcast forwarder for process output to stdout.
- **Filter/scroll mismatch**: `filtered_entries` passed to scroll functions only applied source_filter, while rendering applied both source + expression filters. Fixed by using `visible_log_lines()` for scroll entries too.
- **Registry not wired to TUI**: TaskRunner didn't inject the registry into TaskContext, so `ctx.tasks()` and `ctx.run()` were unavailable in TUI mode. Fixed by threading the Arc<Registry> through App → AppState → TaskRunner → TaskContext.
- **Stale codegen cache**: The installed `runme` binary generates runner code; after changing codegen, the old binary produces stale runners. Must reinstall the CLI binary and clear cache after codegen changes.

## Decisions Log

- **`TaskFn` lifetime**: Used `for<'a>` higher-ranked lifetime so the future borrows from `&TaskContext` but not from `&[String]`, allowing args to be temporary.
- **`ctx.tasks()` returns `Option<TaskQuery>`**: Contexts without a registry (tests, standalone) return None rather than panicking.
- **`ctx.println()` API**: Added as the correct way to emit raw, undecorated text from tasks. Routes through the output buffer so it works in TUI, CLI, and cross-invocation contexts. Replaces direct `println!()` which is invisible in TUI mode.
- **`UiHint` on TaskDef**: Tasks can declare a preferred UI mode (e.g., `ui_hint: Some(UiHint::Cli)` for utility tasks like `list`). Dispatch priority: explicit `--ui` flag > task hint > terminal detection. The `--ui` arg is optional (no default) to distinguish user intent from defaults.
- **Built-in tasks use manual registration**: `#[runme::task]` generates `::runme::` paths that don't resolve inside the `runme` crate itself, so `builtin.rs` uses `inventory::submit!` directly.
- **TTY fallback**: When `--ui tui` is requested but stdout isn't a terminal, automatically falls back to CLI mode.

## Blockers

None — ready to proceed to Phase 4 (integration examples) and final validation.

## Resume Point

Phase 4 tasks remaining:
- **#9 integration-example**: Create/update example RUNME.rs files demonstrating args, cross-invocation, steps
- **#10 final-validation**: End-to-end validation (HUMAN REVIEW gate)

Also not yet done:
- Macro support for `default_ui = "cli"` attribute (currently only settable via manual TaskDef construction)
- The `completions` built-in task (mentioned in plan but not implemented)
