# Runme System Design

## Philosophy

- **Code, not config.** No YAML, no DSLs, no config files. RUNME.rs files are just Rust code. If you need a graph of dependencies, write it. If you need conditional logic, write it. The power of a real language without pretending to be something else.
- **Executable documentation.** Replace READMEs full of commands with code that actually runs. The RUNME.rs _is_ the documentation.
- **AI-maintained richness.** With AI agents as the primary authors of RUNME.rs files, the cost of maintaining rich, explicit task definitions drops dramatically. When designing a new service or feature, updating the runme files is just part of the plan — the agent knows the system design and the runme conventions. This means we can afford far more detailed orchestration than anyone would bother writing by hand in YAML.
- **Works where you work.** RUNME.rs files live anywhere in your directory tree — inside projects, above projects, across repos. The tool assembles capabilities from wherever they're defined.

## Architecture Overview

### RUNME.rs Files

Each RUNME.rs is a Rust source file with a shebang pointing at the `runme` binary. It depends on the `runme` library crate for task definition, runtime, TUI, and utilities.

```rust
#!/usr/bin/env runme

use runme::prelude::*;

#[runme::task(watch = "src/**/*.rs")]
fn build(ctx: &TaskContext) {
    ctx.exec("cargo build");
}

#[runme::main]
fn main() {}
```

### Compilation Model

The `runme` binary owns the compile-and-run pipeline:
1. Discovers RUNME.rs file(s) in the directory tree
2. Content-hashes the source to check for changes
3. If changed (or first run): generates a real Cargo project in a cache directory (`~/.cache/runme/<hash>/`), runs `cargo build`
4. If unchanged: execs the cached binary directly (instant startup)

This means RUNME.rs files are just Rust code — no special syntax, no nightly features, no cargo-script. The `runme` tool handles all the build machinery transparently. The `runme` library dependency is auto-injected into the generated Cargo.toml.

### Multi-File Compilation

When multiple RUNME.rs files exist in a directory tree, they are all compiled into a **single binary**. The TUI, log engine, process management, and task runtime live in the `runme` library crate, which gets compiled into this binary. Everything runs in-process — no cross-process serialization boundary. The existing `Registry`/`OutputBuffer`/`LogStore` model works as-is.

#### Generated Workspace

Each discovered RUNME.rs becomes its own crate in a generated Cargo workspace:

```
~/.cache/runme/<hash>/
├── Cargo.toml           # workspace manifest
├── root/
│   ├── Cargo.toml       # lib crate for ./RUNME.rs
│   └── src/lib.rs       # stripped RUNME.rs source (no main)
├── services_auth/
│   ├── Cargo.toml       # lib crate for services/auth/RUNME.rs
│   └── src/lib.rs
├── web_app/
│   ├── Cargo.toml       # lib crate for web-app/RUNME.rs
│   └── src/lib.rs
└── runner/
    ├── Cargo.toml       # bin crate — depends on all the above
    └── src/main.rs      # imports all crates, builds unified Registry, runs TUI
```

1. **Discover** all RUNME.rs files in the tree (walk up, then walk down).
2. **Generate a workspace** in the cache directory. Each RUNME.rs becomes a library crate. Its `main()` / `#[runme::main]` is stripped — it exports only its `inventory`-registered tasks.
3. **Each RUNME.rs crate** depends on the `runme` library and declares its own frontmatter dependencies. Dependency collisions between files are isolated by crate boundaries — each crate can use different versions of the same dependency.
4. **The runner crate** depends on all RUNME.rs crates. Because `inventory` uses linker sections, all `TaskDef` registrations from all crates end up in the same global collection at link time. The runner's `main()` calls `Registry::from_inventory()` and gets every task from every file. No explicit wiring needed.
5. **Content-hash** covers all source files combined. Cache invalidation recompiles only when any source changes.

#### Path Dependencies

RUNME.rs files can declare path-relative dependencies in their frontmatter:

```rust
//! [dependencies]
//! monorepo-tools = { path = "../shared/tools" }
```

Once the source is copied into the cache directory, these relative paths would be broken. The code generator resolves this by rewriting path dependencies at generation time: the relative path is resolved against the **original RUNME.rs file's location** to produce an absolute path, which is written into the generated crate's `Cargo.toml`. The source code itself is unchanged.

Cache invalidation for path dependencies: the content hash of the RUNME.rs file alone is not sufficient — changes to the path dependency's source would not trigger recompilation. Options:
- Lean on Cargo's own incremental compilation: the generated project points at the real path on disk, so `cargo build` will detect source changes in path dependencies even if our hash says "cached." This requires separating "regenerate the Cargo project" (only when RUNME.rs files change) from "rebuild" (always run `cargo build`, let Cargo decide).
- Include path dependency contents in the hash (expensive for large shared crates).

The first option (let Cargo handle it) is simpler and likely sufficient. The cost is running `cargo build` on every invocation, but Cargo's own caching makes this a no-op when nothing changed.

#### Task Grouping

Tasks need to know which RUNME.rs they came from so the UI can group and namespace them. This requires a `group` field on `TaskDef`:

```rust
pub struct TaskDef {
    pub name: &'static str,
    pub description: Option<&'static str>,
    pub group: &'static str,           // relative path of the RUNME.rs file
    pub watch: Option<&'static str>,
    pub depends_on: &'static [&'static str],
    pub func: TaskFn,
}
```

**Default**: the `#[task]` macro populates `group` automatically. The code generator can inject the relative path (e.g., `services/auth`) as a constant that the macro references. Alternatively, `file!()` gives the compile-time source path, which the macro can normalize.

**Override**: a RUNME.rs file can set a human-friendly name for its group via a library API (e.g., `runme::set_group_name("Auth Service")` or an attribute on `#[runme::main]`). When set, this overrides the path-based default. The exact API is TBD.

The root RUNME.rs group defaults to `.` or the project directory name.

### The `runme` CLI

An installed binary. Handles discovery, compilation, and dispatch:
1. Walk the directory tree to find the relevant RUNME.rs file(s)
2. Compile if needed (generates a Cargo project, runs `cargo build`, caches the result)
3. Exec the compiled binary, passing through arguments

Also provides:
- `runme init` — scaffold a new RUNME.rs for the current project (auto-detect cargo, npm, etc.)
- `runme list` — show available tasks from the current position in the tree

Also serves as the shebang interpreter: `#!/usr/bin/env runme` makes RUNME.rs files directly executable.

### Directory Tree & Discovery

RUNME.rs files form a hierarchy rooted in the filesystem:

```
~/Code/
  RUNME.rs          ← cross-repo orchestration
  services/
    RUNME.rs        ← service-level tasks
    auth/
      RUNME.rs      ← auth-specific tasks
    gateway/
      RUNME.rs
  web-app/
    RUNME.rs        ← frontend tasks
  web-admin/
    RUNME.rs
```

- Running `runme` from a directory uses the nearest RUNME.rs (walking up if needed)
- A RUNME.rs can discover and invoke child RUNME.rs files in subdirectories
- Parent tasks are accessible from child contexts
- Cross-repo orchestration is a first-class use case (e.g. build wasm in one repo, copy artifacts to another, start servers across both)

## Configuration Side

### Task Definition API

Tasks are defined as annotated functions in RUNME.rs files. The API should support:

- **Self-documenting tasks** — every task carries a description that surfaces in `--help` and in the TUI. Derive macros on enums/structs can generate this automatically.
- **Common patterns as conventions** — the library provides reusable building blocks:
  - Watch files and re-run on change
  - Run a subprocess with correct child process management
  - Time-limit an action
  - Run tasks in parallel (e.g. start API server + web server together)
  - Dependency chains (build before test)
- **Smart defaults** — `runme init` and/or built-in tasks can auto-detect the project type (cargo, npm, etc.) and provide sensible `run`, `build`, `test`, `clean` commands out of the box.
- **Rust-native API surface** — use enums, structs, and derive macros to define available commands. The type system documents the API; `#[derive(RunmeTask)]` or similar makes a struct self-describing for CLI and TUI.

### Dual Interface

All tasks work in both modes:
- **CLI mode** — `runme <task> [args]`, `--help` for discovery, structured output for scripting/agents
- **TUI mode** — `runme` with no args (or `runme --tui`), ratatui-based interface for interactive navigation, log viewing, task management

## Usage Side

### Users

Three distinct personas, all using the same tool:

**Power user (developer)** — 20+ year career, deep systems knowledge, constantly changing what they need to run. In the middle of building something and needs to do gymnastics to test it at this stage. Wants full control, visibility into logs, ability to compose ad-hoc task combinations. The TUI is their primary interface. Likely running novel, complex, one-off orchestrations that change day to day.

**AI agent** — benefits from tight build/test/iterate loops. Stable, well-defined commands reduce permission dialog fatigue and keep token usage low. Needs structured output without cruft. The CLI (and eventually MCP) is its primary interface. Giving the agent good runme commands means it can operate more autonomously and produce better results.

**Non-technical colleague** — product managers, designers, etc. using Claude Code to explore UI ideas live in the product. They don't know how anything works and don't want to. They just need "make it run locally" to work. The TUI should be approachable enough that, with an agent's guidance, they can start services and see their changes without understanding the dependency graph underneath.

### Design Principles

- **Fast.** Both in response time and in shortcuts offered. This is a QoL tool — every interaction should feel snappy.
- **Keyboard focused.** Mouse support is fine but keyboard is primary.

### Interaction Phases

There are two distinct phases:

1. **Activation** — the user specifies what they want to run
2. **Interaction** — the user monitors, stops, restarts, and manages running tasks

#### Activation: CLI Mode

Follows standard CLI conventions. Actions and options are generated from the RUNME.rs files.

```
runme start
runme start webscaffold --env staging
```

- Terminal autocomplete support (bash/zsh/fish completions generated from task definitions)
- Structured, predictable command format for both humans and agents

#### Activation: TUI Mode

Fuzzy-find style interface for discovering and selecting tasks without knowing the options in advance.

```
runme
[TUI starts]
sweb<tab>st    ← fuzzy match to "start webscaffold --env staging"
```

- Fuzzy search across task names, descriptions, and options
- Tab completion within the TUI
- History — common to re-run the same commands frequently, so recent/frequent commands should surface quickly
- Two-step: pick the task, then specify options (but fast enough that it feels like one motion)
- Lots of room to explore this interaction — the goal is that the user can find and launch what they need in a few keystrokes

#### After Activation: The Fork

Once a task is triggered, behavior depends on mode:

- **TUI mode** — transitions to a task management UI: live logs, status, stop/restart controls, ability to drill into individual tasks or see interleaved output. Full interactive experience.
- **CLI/shell mode** — only stdio to work with. Multiple output format options:
  - Agent mode (structured, minimal, machine-readable)
  - Log-only mode (streaming stdout/stderr)
  - Piping-friendly formats (JSON lines, etc.)
  - Whatever makes sense for the consumer on the other end

### Log Viewing & Exploration

Logs are a first-class concept, not just raw stdout. Structured JSON logs (typical local dev output) are automatically parsed into structured messages for richer display and interaction.

#### Capabilities

- **Structured parsing** — detect and parse JSON log lines into fields (level, message, service, timestamp, trace ID, etc.). Display them formatted rather than raw.
- **Filtering** — show/hide by process, log level, field values. Filter the live stream without dropping underlying data (filters are a view, not destructive).
- **Multi-source composition** — "show me logs from this process and that one, only errors from this one, hide these ones entirely." A menu/command interface for building up the view you want.
- **Search** — full text search across captured log output, both historical and live stream.
- **Grouping** — group by field values (e.g. group by service, by trace ID).

#### Applies to Both Modes

This isn't TUI-only. CLI/stdio mode supports the same filtering semantics:
- An agent adds debug logging, then runs: `runme start mytask --filter 'level=debug AND service=auth' --context 5` — "only output these log events, with surrounding context"
- Structured output flags control what fields appear, format (JSON lines, human-readable, etc.)
- The agent can be precise about what it needs to see, keeping token usage low while still getting the diagnostic information it's after

### Task Lifecycle & Runtime Visibility

#### Code Defines Behavior, UI Exposes It

The orchestration logic — watch triggers, rebuild chains, file copies, service restarts — lives in the RUNME.rs code. But the runtime should make that behavior visible and interactive.

Example: `runme start webscaffold` might define in code:
1. Watch `web-client/src/**/*.rs`
2. On change → rebuild wasm bundle
3. Copy artifacts to `web-app/public/`
4. Restart the dev server

This is code. But at runtime, the user (or agent) should be able to see this pipeline, its current state, and interact with it:
- See which step is active, which are waiting
- Manually trigger a rebuild without waiting for a file change
- Pause the watch temporarily
- Restart a single step without restarting the whole chain
- Stop one task in a parallel group without tearing down the others

#### Log Re-streaming & Export

All task output is captured in a buffer. At any point — while running or after the fact — the user should be able to selectively re-stream or dump output from specific tasks.

Use cases:
- Start the full setup, then pipe one task's output to another tool (grep, jq, a custom script)
- Run for a while, produce an interesting situation, then dump the relevant logs for sharing or analysis
- An agent requests a replay of the last N seconds of a specific service's output

Possible mechanisms:
- Named FIFOs per task that external tools can read from
- A `runme logs <task>` subcommand that taps into the live buffer or replays history
- Export commands in the TUI (dump to file, copy to clipboard, pipe to command)

The key idea: output is always captured, and you can get at it after the fact without having planned ahead.

#### Open Question

The exact boundary between "defined in code" and "exposed at runtime" is still being worked out. The goal is that writing the code is sufficient — you shouldn't have to write the orchestration *and* separately write UI metadata for it. The runtime should be able to introspect the task graph and make it interactive automatically. How much runtime control to expose (and how) is a design problem to solve as we build.

## AI Agent Integration

### Agent Needs

- **Stable command surface** — `runme test`, `runme start` work everywhere. Consistent interface means permissions can be granted against `runme` commands rather than arbitrary bash. Cleaner trust surface, less permission dialog fatigue.
- **Filtered, structured output** — request only what's needed (errors only, specific log levels, JSON format). Token efficiency directly translates to agent speed and accuracy.
- **Readiness signals** — "start this and tell me when it's accepting requests." Health checks defined in task code, surfaced as clear ready/failed status to the caller.
- **Fire-and-forget with a handle** — start a long-running task, get back an ID, do other work, check on it or grab output later.
- **Discovery** — `runme list --format json` returns every available task, its description, arguments, and options. Enough for the agent to pick the right command without reading source.
- **Timeouts as a first-class argument** — `runme start mytask --timeout 30s`. Runaway processes are a major agent failure mode.

### Dual Interface: CLI + MCP

Both shell commands and MCP tools are useful. They serve different moments in the workflow.

**CLI (shell)** — for launching tasks, simple queries, and pipeline-style interaction. The agent runs commands and reads stdout.

**MCP** — for richer, structured interaction with running tasks. Querying logs, inspecting state, sending signals — things that benefit from a typed request/response model rather than string parsing.

#### Example Workflow

```
# Agent starts the full stack in agent mode (minimal, event-only output)
> runme start --agent-mode
< mongo started
< application server started
< ready for traffic

# Agent does some work, hits a bug, needs diagnostic info
# Uses MCP for structured log queries against the running session
> mcp runme_search_logs [{type: error, dedupe: true}, {filter: "debug log text", context: 10}]
< (matching log entries as JSON)

# Agent makes fixes, wants to trigger a reload without full restart
> SIGHUP to running process
< (tasks reload/restart as defined by their signal handlers)
```

The CLI is the entry point and the blunt instrument. MCP is the scalpel for interacting with running state. Both operate against the same underlying runtime.

### Agent-Friendly Design Principles

- **Non-interactive by default in CLI mode** — no prompts, no "press any key." Do the thing, emit output, exit (or stay running with status events).
- **Meaningful exit codes** — clear success/failure signals, not just 0/1.
- **Quiet by default, verbose on request** — agent mode emits only state transitions and errors unless asked for more. The agent controls the verbosity.
- **Deterministic output format** — `--format json` everywhere. No surprise human-readable decorations mixed into machine output.

## Command API

### Commands as Values

A command is a value — a complete description of a process to run — that can be built, passed around, and ultimately executed through a `TaskContext`. This separates _what to run_ from _how to run it_, giving task authors flexibility in how they compose and reuse commands.

```rust
#[runme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    let cmd = Cmd::new("cargo")
        .args(["build", "--release"])
        .env("RUSTFLAGS", "-C target-cpu=native")
        .cwd("./crates/server");
    ctx.exec(cmd).await?;
    Ok(())
}
```

The `Cmd` type is runme's own. It carries everything needed to describe a process: program, arguments, environment, working directory. Arguments are structured (no shell involved), so interpolation and escaping are non-issues — values are passed directly to the OS.

### Shell Strings

For one-liners and cases where shell features are genuinely wanted (pipes, globbing, redirects), a shell-string path remains available:

```rust
// These are equivalent
ctx.exec(Cmd::shell("cargo build && cargo test")).await?;
ctx.exec("cargo build && cargo test").await?;  // convenience: &str → Cmd::shell()
```

`Cmd::shell()` wraps the string in `sh -c`. This is the escape hatch, not the default. It reintroduces shell escaping concerns, but sometimes that's what you want.

### Conversion from `std::process::Command`

For users who already know the stdlib API or need something runme's builder doesn't expose:

```rust
let mut std_cmd = std::process::Command::new("cargo");
std_cmd.arg("build").env("CARGO_INCREMENTAL", "0");

let cmd = Cmd::from(std_cmd);
ctx.exec(cmd).await?;
```

The conversion extracts what `std::process::Command` carries (program, args, env) into a runme `Cmd`. From there the full builder API is available — you can keep chaining runme-specific methods after conversion:

```rust
let cmd = Cmd::from(std_cmd)
    .cwd("./subdir")
    .env("EXTRA", "yes");
```

### Design Decisions

**`Cmd` is a pure value — no runtime behavior.** `Cmd` owns program, args, env, and cwd. Timeout, readiness checks, output expectations, and retry/restart policy live on the execution side (task definition or execution call), not on the command itself. This keeps `Cmd` simple and composable.

**Working directory** is relative to the RUNME.rs file's location, since that's where the code lives.

**Environment inheritance** is overlay: `Cmd` starts with the parent process's full environment and adds/overrides from `.env()` calls. You rarely want to strip `PATH`.

## Child Process Failure Modes

Runme manages child processes on behalf of task authors. Programs misbehave in many ways; runme should handle all of them gracefully. This checklist tracks known failure modes and our test coverage.

### Won't Die
- [x] Ignores SIGTERM (custom signal handler or blocked signals) — `test_misbehave_ignores_sigterm`
- [x] Forks a child that outlives the parent (orphan processes) — `test_misbehave_orphan_child`, `test_process_group_cleanup`
- [ ] Double-forks to daemonize / escapes the process group
- [ ] Changes its own process group (escapes group signal delivery)

### Won't Finish
- [x] Hangs forever (deadlock, infinite loop, blocked on I/O) — `test_misbehave_hangs_forever`
- [x] Closes stdout/stderr but keeps running (our reader thinks it's done, process lingers) — `test_misbehave_closes_stdout_keeps_running`

### Dies Badly
- [x] Segfault / SIGBUS / SIGABRT (no exit code, just a signal) — `test_misbehave_segfault`
- [ ] OOM-killed by the OS
- [x] Killed by external signal unrelated to us — `test_misbehave_killed_externally`

### Output Problems
- [x] Produces massive output (memory pressure on ring buffer / readers) — `test_misbehave_massive_output`
- [x] Writes extremely long lines with no newlines (line buffering assumptions) — `test_misbehave_long_line`
- [ ] Writes binary / non-UTF8 data — `test_misbehave_binary_output` (known issue: line reader requires UTF-8, binary data silently dropped)
- [ ] Interleaves stdout and stderr in ways that lose ordering

### Zombie / Resource Leaks
- [ ] Exits but leaves zombie children (we need to reap)
- [ ] Holds a port or file lock that blocks the next task
