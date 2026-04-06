# Runme Implementation Plan

## Context

Building a code-based task runner in Rust. Replaces config-file runners (make, npm scripts) with executable Rust files (RUNME.rs) that define tasks as code. Three target users: power developer (TUI), AI agent (CLI/MCP), non-technical colleague (guided TUI). See `docs/system_design.md` for full design.

Key architectural shift: `#!/usr/bin/env runme` instead of cargo-script's nightly shebang. The `runme` binary owns the entire compile-and-run pipeline.

## Crate Structure

```
runme/
├── Cargo.toml              (workspace root)
├── crates/
│   ├── runme-macros/       (proc-macro crate: #[task], #[main], derives)
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   ├── runme/              (library: runtime, task system, TUI, log engine)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── prelude.rs
│   │       ├── task.rs     (Task, TaskContext, Registry)
│   │       ├── process.rs  (child process management)
│   │       ├── log.rs      (structured log parsing, ring buffers)
│   │       ├── watch.rs    (file watching)
│   │       └── tui/        (ratatui UI)
│   └── runme-cli/          (binary: discovery, compilation, dispatch)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── discover.rs (find RUNME.rs files using `ignore` crate)
│           └── compile.rs  (compile, cache, and exec RUNME.rs files)
├── docs/
│   └── system_design.md
└── examples/
    └── RUNME.rs            (example runme file)
```

## Phases

### Phase 1: Workspace & Core Types
**Goal:** Compilable workspace with the fundamental type system that everything builds on.

- Set up Cargo workspace with three crates
- Define core types in `runme` lib:
  - `Task` — name, description, function pointer or closure, metadata (watch patterns, deps, etc.)
  - `TaskContext` — handle passed to task functions. Provides exec, logging, signaling, sibling access.
  - `Registry` — collects tasks, resolves dependencies, provides lookup/listing
- Stub `#[runme::task]` and `#[runme::main]` attribute macros in `runme-macros` using `syn` + `quote`
  - `#[task]` transforms an annotated fn into a registered task with metadata
  - `#[main]` generates the entry point: builds registry, parses CLI args, dispatches
- Re-export macros from `runme` lib via `pub use runme_macros::*`
- Write a minimal RUNME.rs example that compiles and runs directly (not via shebang yet)

**Key dependency:** `syn`, `quote`, `proc-macro2`

### Phase 2: Discovery & Compilation Pipeline
**Goal:** `runme` CLI can find RUNME.rs files and compile/run them.

- Implement RUNME.rs discovery in `runme-cli`:
  - Walk up from cwd to find nearest RUNME.rs
  - Walk down to discover child RUNME.rs files
  - Use `ignore` crate (respects .gitignore)
- Implement compilation pipeline:
  - `runme` generates a real Cargo project in a cache directory (`~/.cache/runme/<content-hash>/`)
  - The generated project depends on the `runme` library crate and includes the RUNME.rs file(s) as source
  - Standard `cargo build` — nothing exotic, just a real Cargo project
  - Content-hash the RUNME.rs source(s) to determine if recompilation is needed
  - Only recompile when RUNME.rs files have actually changed — otherwise exec the cached binary directly
  - This means first run has a compile cost, but subsequent runs are instant
- Dependency frontmatter: RUNME.rs files may need crates beyond `runme` itself. Support an optional frontmatter block for declaring additional dependencies:
  ```rust
  #!/usr/bin/env runme
  //! [dependencies]
  //! reqwest = "0.12"
  //! serde_json = "1"
  ```
  The compilation pipeline parses this and adds them to the generated Cargo.toml alongside the auto-injected `runme` dependency.
- Shebang support: `#!/usr/bin/env runme` — the OS invokes `runme` with the file path, runme compiles (if needed) and runs it
- `runme init` — generate a starter RUNME.rs for the current project (auto-detect cargo/npm/etc.)
- `runme list` — discover tasks and print them (compile the RUNME.rs, exec with `--list` flag handled by `#[main]` macro)

**Key dependencies:** `ignore`, `sha2` or similar for cache hashing

### Phase 3: Process Management
**Goal:** Reliable primitives for running, capturing, and controlling child processes.

- `TaskContext::exec()` — run a command, capture stdout/stderr into ring buffer, stream to log engine
- Process group management — spawn children in process groups for clean shutdown
- Signal handling:
  - Forward signals (SIGINT, SIGTERM, SIGHUP) to child processes
  - SIGHUP for reload semantics (user or agent can trigger re-run without full restart)
  - Clean shutdown: stop children in reverse dependency order
- Output capture:
  - Per-task ring buffer (bounded, configurable size)
  - Structured log detection: if a line parses as JSON, store it structured
  - Raw fallback for non-JSON output
- Parallel execution: run multiple tasks concurrently via tokio::spawn
- Health checks: tasks can declare a readiness condition (HTTP endpoint, log line match, port open)

**Key dependencies:** `tokio` (runtime, process, sync, signal), `nix` (process groups, signals)

### Phase 3b: Command API
**Goal:** Structured command builder that separates "what to run" from "how to run it." See `docs/system_design.md` § Command API.

- `Cmd` type in `crates/runme/src/cmd.rs` — program, args, env overlays, working directory
- `Cmd::new("program").args([...]).env("K", "V").cwd("./path")` builder API
- `Cmd::shell("string")` — wraps in `sh -c` for pipes/globs/redirects
- `&str` → `Cmd::shell()` implicit conversion so `ctx.exec("echo hi")` still works
- `From<std::process::Command>` via `get_program()`, `get_args()`, `get_envs()`
- `ctx.exec()` and `ctx.spawn()` accept `impl Into<Cmd>`
- Cmd is a pure value — no timeout, retry, or runtime behavior (those live on the execution side)
- Working directory relative to RUNME.rs file location
- Environment inherits from parent process, overlays additions

### Phase 4: CLI Interface
**Goal:** Fully functional CLI for humans and agents.

- Argument parsing with `clap` (derive mode)
- Commands: `runme <task> [args]`, `runme list`, `runme init`
- Global flags: `--format json|text`, `--timeout <duration>`, `--agent-mode`
- Task-specific args generated from task metadata
- Shell completion generation (bash/zsh/fish) via `clap_complete`
- Agent mode:
  - Quiet by default, emits only state transitions and errors
  - `--format json` for structured output
  - Meaningful exit codes
  - `--filter` for log filtering on stdout
- `runme logs <task>` — tap into running task's log buffer (or replay history)

**Key dependencies:** `clap`, `clap_complete`

### Phase 5: Log Engine & Structured Output
**Goal:** First-class log viewing, filtering, and export.

- Structured log parser: detect JSON lines, parse into typed fields (level, message, service, timestamp, etc.)
- Filter engine: expressions like `level=error AND service=auth`
- Multi-source composition: combine/filter logs from multiple tasks
- Search: full-text across captured output
- Re-streaming:
  - Named FIFOs per task for external consumption
  - `runme logs <task> --follow` for live tailing
  - Export/dump captured logs to file
- Context windows: `--context N` to include N lines around matches

### Phase 6: TUI — Exploration Phase
**Goal:** Prototype the TUI interaction model. This phase is exploratory.

- Basic ratatui app shell with tokio async event loop
- Activation UI: fuzzy finder for task selection (explore interaction models)
- Task list panel: running/stopped/errored status
- Log viewer panel: scrollable, searchable
- Split/tab views: interleaved vs. per-task log display
- Keyboard-driven: vim-style navigation, command palette
- History: recent commands, frequent commands surface first
- Runtime task controls: stop, restart, trigger, pause watch

**Key dependencies:** `ratatui`, `crossterm`, `tui-input` or similar, `nucleo` or `fuzzy-matcher` for fuzzy find

**Open questions (to explore):**
- How much of the log filtering UI is modal (menu) vs. inline (command bar)?
- What's the layout? Sidebar task list + main log view? Tabs? Splits?
- How do watch-triggered rebuilds surface? Toast notification? Status indicator? Log entry?

### Phase 7: Introspectable Composability — Exploration Phase
**Goal:** Find the right boundary between "it's just code" and "the runtime can see the structure."

This is the hardest design problem. The task function is imperative Rust code, but the TUI/agent needs to see and manipulate the steps within it.

Ideas to explore:
- **Builder-pattern pipelines**: `ctx.watch("...").then(|| ...).then(|| ...)` creates a describable chain
- **Named steps**: `ctx.step("build wasm", || { ... })` — each step is a labeled unit the runtime can display and control
- **Reactive graph**: tasks emit events, the runtime wires them together. More like a dataflow system than imperative code.
- **Hybrid**: the coarse structure (task deps, watch triggers, parallel groups) is declarative via macro attributes; the fine structure within a task is imperative code with optional step annotations

The goal: writing code is sufficient. You shouldn't have to write orchestration AND separately write UI metadata. But the runtime should be able to introspect enough to make it interactive.

### Phase 8: AI Agent Integration
**Goal:** MCP server and agent-optimized workflows.

- MCP server exposing tasks as tools:
  - `runme_list` — discover available tasks
  - `runme_start` — start a task, return a handle
  - `runme_status` — check task state
  - `runme_logs` — query logs with filters
  - `runme_stop` / `runme_signal` — control running tasks
- Prompt/skill documents: teach agents how to create, interpret, and execute RUNME.rs files
- Token optimization: filtering, summarization, structured output that minimizes tokens while preserving signal

## Suggested Starting Order

1. **Phase 1** (core types + macros) — foundation everything depends on
2. **Phase 3** (process management) — the hardest systems-level work, good to prove out early
3. **Phase 2** (discovery + compilation) — connects the pieces: find a file, compile it, run it with the process primitives
4. **Phase 4** (CLI) — first usable end-to-end experience
5. **Phase 5** (log engine) — needed before TUI but also useful in CLI
6. **Phase 6** (TUI) — exploratory, builds on everything above
7. **Phase 7** (introspection) — exploratory, can happen in parallel with TUI
8. **Phase 8** (AI integration) — builds on stable CLI + log engine

Phases 6 and 7 are explicitly exploratory — expect iteration and rework. The earlier phases should be built solidly since everything depends on them.

## Verification

After each phase:
- Phase 1: A RUNME.rs example with `#[task]` and `#[main]` compiles and runs, tasks execute
- Phase 2: `runme` discovers and runs a RUNME.rs file from anywhere in a directory tree
- Phase 3: A task can start a long-running process, capture its output, stop it cleanly
- Phase 4: `runme list`, `runme <task>`, `runme <task> --format json` all work. Shell completions generate.
- Phase 5: Structured JSON logs are parsed, filterable, searchable, re-streamable
- Phase 6: TUI launches, shows tasks, displays logs, fuzzy find works
- Phase 7: Task pipelines are visible and manipulable at runtime
- Phase 8: An AI agent can discover, run, and query tasks via MCP
