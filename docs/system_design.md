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
    .timeout(Duration::from_secs(30));
```

### Open Questions

**What belongs on `Cmd` vs elsewhere?** The command itself clearly owns program, args, env, cwd. But what about:
- **Timeout** — is it a property of the command ("this should never run longer than 30s") or of the execution context ("run this with a 30s timeout")?
- **Readiness checks** — does a command know how to tell you it's ready (e.g. "wait for port 8080"), or is that a separate concern layered on top?
- **Output expectations** — should a command declare that it produces JSON logs, or is that a runtime interpretation?
- **Retry/restart policy** — "restart on failure" feels like orchestration, not command description.

Putting everything on `Cmd` makes it a self-contained unit of work. Keeping `Cmd` lean and putting runtime behavior on the execution call (or on the task definition) keeps the type simple and composable. There's a spectrum here.

**Working directory semantics.** `.cwd()` relative to what? The RUNME.rs file's location? The directory `runme` was invoked from? The project root? Needs a clear convention, probably relative to the RUNME.rs file since that's where the code lives.

**Environment inheritance.** Does a `Cmd` start with the parent's full environment and overlay, or start empty? Overlay is almost certainly right (you rarely want to strip `PATH`), but worth being explicit.

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
