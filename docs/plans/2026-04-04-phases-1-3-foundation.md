# Runme Foundation: Phases 1-3

## Goal

Stand up the foundational crates, core type system, proc macros, RUNME.rs discovery/compilation pipeline, and process management primitives. At the end, `runme` can discover a RUNME.rs file, compile it, and execute tasks that spawn and manage child processes with output capture.

## Approach

Phase 1 (workspace + types + macros) must complete first — everything depends on it. Then Phase 2 (discovery/compilation in the CLI crate) and Phase 3 (process management in the library crate) run in parallel since they touch different crates with no file contention.

```
Phase 1: Workspace & Core Types ─── single implementor (tightly coupled)
    │
    ├── validate Phase 1
    │
    ├─── Phase 2: Discovery & Compilation ─── single implementor
    │         │
    │         └── validate Phase 2
    │
    └─── Phase 3: Process Management ─── single implementor
              │
              └── validate Phase 3
                        │
                        └── Integration validation (end-to-end)
```

## Acceptance Criteria

- [ ] Cargo workspace compiles with three crates: `runme-macros`, `runme`, `runme-cli`
- [ ] `#[runme::task]` and `#[runme::main]` macros work — annotated functions register as tasks
- [ ] `Task`, `TaskContext`, `Registry` types are defined with appropriate traits
- [ ] Example RUNME.rs compiles and executes tasks by name
- [ ] `runme-cli` discovers RUNME.rs files walking up and down the directory tree
- [ ] Compilation pipeline: generates Cargo project in cache dir, content-hashes to skip rebuilds, execs cached binary
- [ ] Dependency frontmatter (`//! [dependencies]`) is parsed and injected into generated Cargo.toml
- [ ] `#!/usr/bin/env runme` shebang execution works
- [ ] `TaskContext::exec()` runs child processes with captured stdout/stderr
- [ ] Output captured into per-task ring buffers with structured JSON log detection
- [ ] Process groups: children spawned in groups, clean shutdown on SIGINT/SIGTERM
- [ ] Signal forwarding: SIGHUP triggers reload semantics
- [ ] Parallel task execution via tokio::spawn
- [ ] `cargo test` passes across all crates

## Human Review Gates

1. **Phase 1 completion** — Review workspace structure, type design, and macro API before building on top of it.
   - Human Review: true
   - Auto-Approve: false
   - Rationale: This is the foundation everything depends on. Wrong type design here cascades everywhere. Worth reviewing the actual API surface.

## Status

- [x] Draft
- [x] Approved
- [x] Phase 1: Complete
- [x] Phase 2 + 3: Complete
- [x] Integration: Complete

## Context

- **Repo:** `/Users/dgrijalva/Code/runme`
- **Current state:** Workspace with 3 crates, 62+ tests passing, working CLI
- **Design docs:** `docs/system_design.md`, `docs/01-implementation-plan.md`
- **Key constraint:** The existing `Cargo.toml` and `src/main.rs` need to be replaced by the workspace structure
- **Rust edition:** 2024 (keep this across all crates)

## Team

| Name | Role | Agent Type | Model | Strategy |
|------|------|-----------|-------|----------|
| `workspace-architect` | Implement Phase 1: workspace setup, core types, proc macros, example | general-purpose | opus | subagent |
| `discovery-builder` | Implement Phase 2: RUNME.rs discovery, compilation pipeline, frontmatter parsing | general-purpose | opus | subagent |
| `process-engineer` | Implement Phase 3: child process management, signals, output capture, parallel execution | general-purpose | opus | subagent |
| `phase1-validator` | Validate Phase 1: workspace compiles, macros work, example runs | general-purpose | sonnet | subagent |
| `phase2-validator` | Validate Phase 2: discovery finds files, compilation caches correctly, shebang works | general-purpose | sonnet | subagent |
| `phase3-validator` | Validate Phase 3: processes spawn/capture/shutdown correctly | general-purpose | sonnet | subagent |
| `integration-validator` | Validate end-to-end: discover, compile, run tasks with process management | general-purpose | opus | subagent |

## Phase 1: Workspace & Core Types

### Task: `phase1-implement`

- **Depends On:** none
- **Assigned To:** `workspace-architect`
- **Parallel:** no (Phase 1 is sequential)
- **Human Review:** true (review before Phase 2+3 proceed)

**Description:**

Set up the runme Cargo workspace and implement the core type system and proc macros. This is the foundation everything else builds on.

**Working directory:** `/Users/dgrijalva/Code/runme`

**Read first:** `docs/system_design.md` and `docs/01-implementation-plan.md` for full context.

**Step 1: Restructure into workspace**

Replace the existing single-crate setup with a Cargo workspace:

```
runme/
├── Cargo.toml              (workspace root — NOT a package)
├── crates/
│   ├── runme-macros/       (proc-macro crate)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   ├── runme/              (library crate)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── prelude.rs
│   │       └── task.rs
│   └── runme-cli/          (binary crate)
│       ├── Cargo.toml
│       └── src/main.rs
├── docs/
└── examples/
    └── RUNME.rs
```

- Root `Cargo.toml` becomes a workspace manifest (remove `[package]`, add `[workspace]` with members)
- Delete `src/main.rs` (replaced by `crates/runme-cli/src/main.rs`)
- All crates use `edition = "2024"`
- `runme-macros` Cargo.toml: `proc-macro = true`, depends on `syn` (features: full, extra-traits), `quote`, `proc-macro2`
- `runme` Cargo.toml: depends on `runme-macros`, `tokio` (features: full), `serde`, `serde_json`
- `runme-cli` Cargo.toml: depends on `runme` library crate, `clap` (features: derive)

**Step 2: Core types in `crates/runme/src/task.rs`**

Define the foundational types:

```rust
// Task metadata — what the macro extracts and registers
pub struct TaskDef {
    pub name: String,
    pub description: Option<String>,
    pub watch: Option<String>,         // glob pattern for file watching
    pub depends_on: Vec<String>,       // task dependency names
    pub func: fn(&TaskContext),        // the task function
}

// Runtime context passed to task functions
pub struct TaskContext {
    pub name: String,
    // TODO Phase 3: will add exec(), log access, sibling handles, etc.
}

// Collects and looks up tasks
pub struct Registry {
    tasks: Vec<TaskDef>,
}

impl Registry {
    pub fn new() -> Self;
    pub fn register(&mut self, task: TaskDef);
    pub fn get(&self, name: &str) -> Option<&TaskDef>;
    pub fn list(&self) -> &[TaskDef];
    pub fn run(&self, name: &str);     // look up task, create context, call func
}
```

Keep these types simple now — they'll be extended in Phase 3 (process management) and beyond. The important thing is getting the shape right.

Export through `crates/runme/src/prelude.rs`:
```rust
pub use crate::task::{TaskDef, TaskContext, Registry};
pub use runme_macros::{task, main as runme_main};
// re-export the macros so RUNME.rs files only need `use runme::prelude::*`
```

And from `crates/runme/src/lib.rs`:
```rust
pub mod task;
pub mod prelude;
pub use runme_macros::{task, main};
```

**Step 3: Proc macros in `crates/runme-macros/src/lib.rs`**

Implement two attribute macros:

**`#[runme::task]`** — transforms a function into a registered task:

Input:
```rust
#[runme::task(desc = "Build the project", watch = "src/**/*.rs")]
fn build(ctx: &TaskContext) {
    // user code
}
```

Should expand to something like:
```rust
fn build(ctx: &TaskContext) {
    // user code
}

// Registration function that #[main] will call
fn __runme_register_build(registry: &mut Registry) {
    registry.register(TaskDef {
        name: "build".to_string(),
        description: Some("Build the project".to_string()),
        watch: Some("src/**/*.rs".to_string()),
        depends_on: vec![],
        func: build,
    });
}
```

The exact registration mechanism needs thought. Options:
- Generate `__runme_register_<name>` functions that `#[main]` collects
- Use `inventory` or `linkme` crate for automatic static collection
- Have the macro generate a static array that `#[main]` concatenates

The simplest approach: `#[main]` scans the module for `#[task]` annotated functions and generates registration calls. Since both macros have access to the same token stream at compile time, `#[main]` can expand to call all `__runme_register_*` functions.

Actually, simplest: use the `inventory` crate pattern — each `#[task]` submits to a global registry via `inventory::submit!`, and `#[main]` collects with `inventory::iter::<TaskDef>`. This avoids the macro needing to know about other macros.

**`#[runme::main]`** — generates the entry point:

Input:
```rust
#[runme::main]
fn main() {}
```

Should expand to:
```rust
fn main() {
    let mut registry = Registry::new();
    // collect all tasks registered via inventory
    for task in inventory::iter::<TaskDef> {
        registry.register(task.clone());
    }

    // Parse CLI args to find which task to run
    let args: Vec<String> = std::env::args().collect();

    // Special handling for --list flag (used by `runme list`)
    if args.iter().any(|a| a == "--list") {
        for task in registry.list() {
            println!("{}: {}", task.name, task.description.as_deref().unwrap_or(""));
        }
        return;
    }

    // Run the named task
    if let Some(task_name) = args.get(1) {
        registry.run(task_name);
    } else {
        // No task specified — list available tasks
        println!("Available tasks:");
        for task in registry.list() {
            println!("  {}: {}", task.name, task.description.as_deref().unwrap_or(""));
        }
    }
}
```

**Step 4: Example RUNME.rs**

Create `examples/RUNME.rs`:
```rust
use runme::prelude::*;

#[runme::task(desc = "Say hello")]
fn hello(ctx: &TaskContext) {
    println!("Hello from task: {}", ctx.name);
}

#[runme::task(desc = "Say goodbye")]
fn goodbye(ctx: &TaskContext) {
    println!("Goodbye from task: {}", ctx.name);
}

#[runme::main]
fn main() {}
```

Verify it works:
```bash
cargo run --example RUNME -- hello
cargo run --example RUNME -- goodbye
cargo run --example RUNME -- --list
```

**Acceptance criteria for this task:**
- `cargo build` succeeds for the entire workspace
- `cargo run --example RUNME -- hello` prints "Hello from task: hello"
- `cargo run --example RUNME -- --list` lists both tasks with descriptions
- `cargo test` passes (add basic tests for Registry)

---

### Task: `phase1-validate`

- **Depends On:** `phase1-implement`
- **Assigned To:** `phase1-validator`
- **Parallel:** no
- **Human Review:** false

**Description:**

Validate Phase 1 implementation:

1. Run `cargo build` — workspace must compile cleanly
2. Run `cargo test` — all tests pass
3. Run `cargo run --example RUNME -- hello` — prints hello message
4. Run `cargo run --example RUNME -- goodbye` — prints goodbye message
5. Run `cargo run --example RUNME -- --list` — lists both tasks with descriptions
6. Verify crate structure matches the plan (three crates in `crates/`)
7. Verify `runme::prelude::*` exports the core types and macros
8. Review the macro expansion — does `#[task]` generate sensible registration code?

Report: list what passes and what doesn't. If anything fails, describe the failure clearly.

---

## Phase 2: Discovery & Compilation Pipeline

### Task: `phase2-implement`

- **Depends On:** `phase1-validate`
- **Assigned To:** `discovery-builder`
- **Parallel:** yes (with `phase3-implement`)
- **Human Review:** false

**Description:**

Implement RUNME.rs file discovery and the compilation/caching pipeline in the `runme-cli` crate. After this, `runme` can find, compile, cache, and execute RUNME.rs files.

**Working directory:** `/Users/dgrijalva/Code/runme`

**Read first:** `docs/system_design.md` and `docs/01-implementation-plan.md` for full context. Also read the Phase 1 implementation to understand the types and macros available.

**Step 1: Discovery (`crates/runme-cli/src/discover.rs`)**

Implement RUNME.rs file discovery using the `ignore` crate:

```rust
pub struct DiscoveryResult {
    /// The nearest RUNME.rs found (walking up from cwd)
    pub nearest: Option<PathBuf>,
    /// All RUNME.rs files found in the subtree (walking down)
    pub children: Vec<PathBuf>,
}

/// Find RUNME.rs files relative to the given directory
pub fn discover(from: &Path) -> DiscoveryResult;
```

- **Walk up:** Starting from `from`, check each ancestor directory for `RUNME.rs` until one is found or we hit the filesystem root
- **Walk down:** From the directory containing the nearest RUNME.rs, walk subdirectories using `ignore::WalkBuilder` to find child RUNME.rs files. This respects `.gitignore` automatically.
- The filename to search for is `RUNME.rs` (exact match, case-sensitive)

Add `ignore` to `runme-cli` dependencies.

**Step 2: Frontmatter parser (`crates/runme-cli/src/frontmatter.rs`)**

Parse the optional dependency frontmatter from RUNME.rs files:

```rust
pub struct Frontmatter {
    pub dependencies: Vec<(String, String)>,  // (name, version spec)
}

/// Parse frontmatter from RUNME.rs source code
/// Looks for doc comments starting with `//! [dependencies]`
pub fn parse_frontmatter(source: &str) -> Frontmatter;
```

Format to parse:
```rust
#!/usr/bin/env runme
//! [dependencies]
//! reqwest = "0.12"
//! serde_json = "1"
```

Rules:
- Skip the shebang line (`#!...`)
- Look for `//! [dependencies]` as a section header
- Parse subsequent `//! name = "version"` lines as TOML-style dependency declarations
- Stop at the first line that isn't a `//!` comment
- If no frontmatter found, return empty dependencies (runme is always auto-injected)

**Step 3: Compilation pipeline (`crates/runme-cli/src/compile.rs`)**

```rust
pub struct CompileResult {
    pub binary_path: PathBuf,
    pub was_cached: bool,
}

/// Compile a RUNME.rs file, returning the path to the resulting binary.
/// Uses content-hash caching to skip recompilation when source hasn't changed.
pub fn compile(runme_file: &Path) -> Result<CompileResult, CompileError>;
```

Pipeline:
1. Read the RUNME.rs source
2. Strip the shebang line (if present) — Rust doesn't understand `#!`
3. Parse frontmatter for extra dependencies
4. Compute SHA-256 hash of the source content
5. Cache directory: `~/.cache/runme/<first-8-chars-of-hash>/`
6. Check if cached binary exists and the stored hash matches → return cached path
7. If not cached or hash changed:
   a. Create the cache directory
   b. Generate `Cargo.toml`:
      ```toml
      [package]
      name = "runme-script"
      version = "0.1.0"
      edition = "2024"

      [dependencies]
      runme = { path = "<absolute-path-to-crates/runme>" }
      # ... any extra dependencies from frontmatter
      ```
   c. Copy the RUNME.rs file as `src/main.rs` (with shebang stripped)
   d. Run `cargo build --release` in the generated project directory
   e. Store the hash in a marker file for future cache checks
   f. Return the path to the compiled binary

Add `sha2` to `runme-cli` dependencies.

**Step 4: CLI dispatch (`crates/runme-cli/src/main.rs`)**

Wire everything together:

```rust
fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Detect shebang invocation: runme is called with a file path as first arg
    // (env passes: runme <script-path> [script-args...])
    let (runme_file, pass_through_args) = if args.len() > 1 && args[1].ends_with(".rs") {
        // Shebang mode: first arg is the script path
        (PathBuf::from(&args[1]), &args[2..])
    } else {
        // Discovery mode: find nearest RUNME.rs
        let cwd = std::env::current_dir().unwrap();
        let result = discover(&cwd);
        match result.nearest {
            Some(path) => (path, &args[1..]),
            None => {
                eprintln!("No RUNME.rs found");
                std::process::exit(1);
            }
        }
    };

    // Compile (or use cached)
    let compiled = compile(&runme_file).unwrap_or_else(|e| {
        eprintln!("Compilation failed: {}", e);
        std::process::exit(1);
    });

    // Exec the compiled binary, replacing this process
    let err = exec::execvp(&compiled.binary_path, pass_through_args);
    eprintln!("Failed to exec: {}", err);
    std::process::exit(1);
}
```

Use the `exec` crate (or `nix::unistd::execvp`) for process replacement.

Add `clap` (derive, for future expansion), `exec` or `nix` to dependencies.

**Step 5: Tests**

- Unit test: `discover()` with a temp directory containing RUNME.rs files at various depths
- Unit test: `parse_frontmatter()` with various inputs (no frontmatter, with deps, with shebang)
- Unit test: `compile()` with a simple RUNME.rs file (verify caching — compile twice, second should be cached)
- Integration: create a RUNME.rs in a temp dir, run `runme-cli` binary, verify task executes

**Acceptance criteria for this task:**
- `runme-cli` discovers RUNME.rs walking up from a subdirectory
- `runme-cli` discovers child RUNME.rs files walking down
- Compilation generates a Cargo project, builds it, caches the binary
- Second run with unchanged source skips compilation (returns cached)
- Frontmatter dependencies are parsed and included in generated Cargo.toml
- Shebang invocation (`runme ./path/to/RUNME.rs hello`) works
- Discovery invocation (`runme hello` from a dir with RUNME.rs) works
- `cargo test` passes for runme-cli

---

### Task: `phase2-validate`

- **Depends On:** `phase2-implement`
- **Assigned To:** `phase2-validator`
- **Parallel:** yes (with `phase3-validate`)
- **Human Review:** false

**Description:**

Validate Phase 2 implementation:

1. Run `cargo test -p runme-cli` — all tests pass
2. Create a temp directory with RUNME.rs files at multiple depths, verify discovery
3. Run `cargo run -p runme-cli -- <path-to-example-RUNME.rs> hello` — task executes
4. Run again — verify it uses cached binary (check output or timing)
5. Create a RUNME.rs with dependency frontmatter, verify the generated Cargo.toml includes it
6. Make the example RUNME.rs executable with `#!/usr/bin/env` shebang, verify direct execution
7. Verify `.gitignore`-ignored directories are skipped during discovery

Report: list what passes and what doesn't.

---

## Phase 3: Process Management

### Task: `phase3-implement`

- **Depends On:** `phase1-validate`
- **Assigned To:** `process-engineer`
- **Parallel:** yes (with `phase2-implement`)
- **Human Review:** false

**Description:**

Implement process management primitives in the `runme` library crate. After this, task functions can spawn child processes with captured output, signal handling, and parallel execution.

**Working directory:** `/Users/dgrijalva/Code/runme`

**Read first:** `docs/system_design.md` and `docs/01-implementation-plan.md`. Also read the Phase 1 implementation to understand TaskContext and Registry.

**Step 1: Extend TaskContext with process execution (`crates/runme/src/process.rs`)**

```rust
use tokio::process::Command;
use tokio::sync::broadcast;

/// Result of a process execution
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,    // captured output
    pub stderr: String,    // captured error output
}

/// Handle to a running process
pub struct ProcessHandle {
    child: tokio::process::Child,
    task_name: String,
    stdout_rx: broadcast::Receiver<LogLine>,
    stderr_rx: broadcast::Receiver<LogLine>,
}

/// A line of captured output, possibly structured
pub enum LogLine {
    Raw(String),
    Structured(serde_json::Value),
}

/// Output ring buffer for a task
pub struct OutputBuffer {
    lines: VecDeque<LogLine>,
    capacity: usize,
    tx: broadcast::Sender<LogLine>,
}
```

Implement on TaskContext:
```rust
impl TaskContext {
    /// Run a command and wait for it to complete. Captures output.
    pub async fn exec(&self, command: &str) -> Result<ExecResult, ProcessError>;

    /// Spawn a long-running command. Returns a handle for monitoring/control.
    pub async fn spawn(&self, command: &str) -> Result<ProcessHandle, ProcessError>;

    /// Access the output buffer for this task
    pub fn output(&self) -> &OutputBuffer;
}
```

`exec()` implementation:
- Parse command string (shell-style splitting, or pass to `sh -c`)
- Spawn via `tokio::process::Command` with piped stdout/stderr
- Read stdout/stderr line by line, push into OutputBuffer
- For each line: try `serde_json::from_str()` — if it parses, store as `Structured`, otherwise `Raw`
- Wait for process exit
- Return ExecResult

`spawn()` implementation:
- Same as exec but don't wait — return the ProcessHandle
- Spawn background tokio tasks to continuously read stdout/stderr into the buffer

Add `tokio`, `serde`, `serde_json` dependencies to the `runme` crate Cargo.toml.

**Step 2: Process groups (`crates/runme/src/process.rs`)**

Spawn children in their own process group so we can signal the whole group:

```rust
use nix::unistd::{setpgid, Pid};
use nix::sys::signal::{killpg, Signal};

// In Command setup, before spawn:
unsafe {
    command.pre_exec(|| {
        setpgid(Pid::from_raw(0), Pid::from_raw(0))?;
        Ok(())
    });
}
```

On ProcessHandle:
```rust
impl ProcessHandle {
    /// Send a signal to the process group
    pub fn signal(&self, sig: Signal) -> Result<(), ProcessError>;

    /// Graceful shutdown: SIGTERM, wait, then SIGKILL if needed
    pub async fn stop(&mut self, timeout: Duration) -> Result<(), ProcessError>;

    /// Check if process is still running
    pub fn is_running(&self) -> bool;

    /// Wait for the process to exit
    pub async fn wait(&mut self) -> Result<ExecResult, ProcessError>;
}
```

Add `nix` (features: signal, process) to dependencies.

**Step 3: Signal handling (`crates/runme/src/signal.rs`)**

Set up signal handlers for the runme process itself:

```rust
use tokio::signal::unix::{signal, SignalKind};

pub struct SignalHandler {
    // tracks all active ProcessHandles
    processes: Vec<ProcessHandle>,
}

impl SignalHandler {
    /// Install signal handlers. On SIGINT/SIGTERM: stop all children gracefully.
    /// On SIGHUP: restart all children (or trigger reload).
    pub async fn install(&self);
}
```

- SIGINT (Ctrl-C): forward to all child process groups, wait for them to exit, then exit
- SIGTERM: same as SIGINT
- SIGHUP: signal children to reload (forward SIGHUP), or stop and restart them

The signal handler should integrate with tokio's signal handling (`tokio::signal::unix::signal`).

**Step 4: Parallel execution**

Extend Registry to support running multiple tasks concurrently:

```rust
impl Registry {
    /// Run multiple tasks in parallel
    pub async fn run_parallel(&self, names: &[&str]) -> Vec<Result<(), TaskError>>;
}
```

Use `tokio::spawn` for each task, `tokio::join!` or `JoinSet` to wait for all.

**Step 5: Make the runtime async**

Since we're adding tokio, the `#[runme::main]` macro expansion needs to set up a tokio runtime. Update the macro (coordinate with Phase 1 types) to expand to:

```rust
#[tokio::main]
async fn main() {
    // ... registry setup and dispatch
}
```

This means TaskContext methods are async, and task functions should be async too. Update the `TaskDef` function pointer type:
```rust
pub func: fn(&TaskContext) -> Pin<Box<dyn Future<Output = ()>>>,
// or use async-trait
```

Consider using `async-trait` crate to make this ergonomic, or use the native async fn in trait (available in edition 2024).

**Step 6: Tests**

- Unit test: `exec()` runs a command, captures stdout
- Unit test: `exec()` detects JSON lines as structured
- Unit test: `spawn()` starts a long-running process, `stop()` shuts it down
- Unit test: process group — spawned child's children also die on stop
- Unit test: `OutputBuffer` ring buffer wraps correctly at capacity
- Unit test: parallel execution of multiple tasks
- Integration test: signal handling — send SIGINT, verify children stop

Write tests that use simple commands (`echo`, `sleep`, `cat`) to verify process management without external dependencies.

**Acceptance criteria for this task:**
- `TaskContext::exec("echo hello")` returns stdout "hello\n"
- JSON output lines are detected and stored as `LogLine::Structured`
- `spawn()` returns a handle, `stop()` terminates the process group
- SIGINT/SIGTERM to the main process triggers child cleanup
- `run_parallel()` executes multiple tasks concurrently
- OutputBuffer captures output with bounded capacity
- `cargo test -p runme` passes

---

### Task: `phase3-validate`

- **Depends On:** `phase3-implement`
- **Assigned To:** `phase3-validator`
- **Parallel:** yes (with `phase2-validate`)
- **Human Review:** false

**Description:**

Validate Phase 3 implementation:

1. Run `cargo test -p runme` — all tests pass
2. Write a quick test script that spawns a process via `TaskContext::exec()`, verify output capture
3. Test structured log detection: exec a command that outputs JSON lines, verify they're parsed
4. Test `spawn()` + `stop()`: start `sleep 100`, then stop it, verify it's killed
5. Test signal handling: spawn a child, send SIGTERM to main, verify child is cleaned up
6. Test parallel execution: run 3 tasks that each print their name, verify all execute
7. Verify OutputBuffer wraps at capacity (fill beyond limit, verify old entries dropped)

Report: list what passes and what doesn't.

---

## Integration Validation

### Task: `integration-validate`

- **Depends On:** `phase2-validate`, `phase3-validate`
- **Assigned To:** `integration-validator`
- **Parallel:** no
- **Human Review:** false

**Description:**

Validate the end-to-end flow: discovery + compilation + process management working together.

1. Create a RUNME.rs that defines tasks using process management:
   ```rust
   #!/usr/bin/env runme

   use runme::prelude::*;

   #[runme::task(desc = "Run echo")]
   async fn echo(ctx: &TaskContext) {
       ctx.exec("echo 'Hello from runme!'").await.unwrap();
   }

   #[runme::task(desc = "Run a background process")]
   async fn background(ctx: &TaskContext) {
       let handle = ctx.spawn("sleep 5").await.unwrap();
       println!("Process spawned, stopping...");
       handle.stop(Duration::from_secs(2)).await.unwrap();
       println!("Stopped cleanly");
   }

   #[runme::main]
   fn main() {}
   ```

2. Place this RUNME.rs in a temp directory
3. Run `cargo run -p runme-cli -- <path>/RUNME.rs echo` — verify output
4. Run `cargo run -p runme-cli -- <path>/RUNME.rs background` — verify spawn/stop
5. Run `cargo run -p runme-cli -- <path>/RUNME.rs --list` — verify task listing
6. Run twice — verify second run uses cached binary
7. Navigate to a subdirectory of the RUNME.rs location, run `cargo run -p runme-cli -- echo` — verify discovery walks up

Report comprehensive results. Flag any issues with the interaction between compilation pipeline and runtime.

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
  clippy:
    command: "cargo clippy --workspace -- -D warnings"
    required: false
  example:
    command: "cargo run --example RUNME -- hello"
    required: true
```

## Findings

- `inventory` crate requires `&'static str` fields on `TaskDef` (not `String`) for `Send + Sync + 'static` compliance
- Blanket `impl<T: Serialize> From<T> for TaskError` works alongside specific `From<ProcessError>` as long as `ProcessError` doesn't implement `Serialize`
- Coherence prevents `TaskError` from implementing `Serialize` (would conflict with the blanket From)
- Binary/non-UTF8 process output is silently dropped by tokio's UTF-8 line reader — test exists as `#[ignore]` TODO
- Debug build mode is ~10% faster than release for RUNME.rs compilation with negligible runtime impact — switched to debug as default
- Phase 2's compile integration test needed manual updating when TaskFn signature changed — fragile because it uses raw `inventory::submit!` instead of the macro

## Decisions Log

- **TaskError design**: struct with `serde_json::Value` output + `ExitHint` enum. Blanket `From<Serialize>` for easy construction, `ResultExt::task_err()` for std error types. TaskError intentionally never implements Serialize or std::error::Error.
- **ExecResult → ExecOutput**: Renamed, dropped exit_code field. Non-zero exit is now `ProcessError::ExitCode { code, output }`. `ExecOutputExt` trait provides `.output()` on `Result<ExecOutput, ProcessError>` to access captured output regardless of success/failure.
- **Doc comments as descriptions**: `#[runme::task]` extracts `///` doc comments as task description, falling back to `desc = "..."` attribute.
- **RUNME.rs build mode**: Debug (not release) — task runner scripts don't need runtime optimization.
- **Output streaming**: Not yet implemented. `ctx.exec()` captures into buffers but nothing streams to terminal. This is the bridge to Phase 5 (log engine). The `install` task currently produces no visible output.
- **Command API**: New section added to system_design.md — `Cmd` type as a value describing a process, with shell string convenience path and `std::process::Command` conversion.

## Blockers

None — Phases 1-3 complete. Next work is Phase 4 (config/args) or Phase 5 (log engine/output streaming).

## Post-Plan Work (same thread)

After plan completion, additional work was done directly with the user:

- **TaskError/ExitHint system** — new error module with structured JSON output and exit hints
- **Proc macro updates** — supports both void and Result return types, doc comment extraction
- **Process management refinements** — exec() auto-errors on non-zero exit, ExecResult→ExecOutput rename
- **Misbehaving process test suite** — 9 tests covering SIGTERM ignoring, orphan children, hangs, segfaults, massive output, binary data, etc.
- **Child Process Failure Modes checklist** — added to system_design.md, partially checked off
- **Command API section** — added to system_design.md by user
- **Project RUNME.rs** — created at repo root with `install` task using ctx.exec()
