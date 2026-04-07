# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Architecture

Runme is a Rust-based task runner where tasks are defined in `RUNME.rs` files — plain Rust source files with a `#!/usr/bin/env runme` shebang. No YAML, no DSLs.

### Workspace Crates

- **`runme`** (`crates/runme/`) — Core library. Contains the task runtime, process management, log engine, TUI, and the prelude that RUNME.rs files import. This is what gets compiled into every generated binary.
- **`runme-cli`** (`crates/runme-cli/`) — The `runme` binary. Handles discovery of RUNME.rs files, workspace generation, compilation, and exec of the resulting binary. Not a runtime dependency — it's the build orchestrator.
- **`runme-macros`** (`crates/runme-macros/`) — Proc macros: `#[runme::task]` and `#[runme::init]`. Generates `TaskDef`/`InitDef` registrations via `inventory`.

### Compilation Pipeline (runme-cli)

The CLI discovers all RUNME.rs files in a directory tree, generates a Cargo workspace in a cache directory, builds it, and execs the resulting binary. Key modules:

1. **`discover.rs`** — Walks up to find the nearest RUNME.rs, then walks down to find children. Respects `.gitignore` via the `ignore` crate.
2. **`transform.rs`** — Source transformation: strips shebang, injects `__RUNME_GROUP` constant.
3. **`frontmatter.rs`** — Parses `//! [dependencies]` frontmatter from RUNME.rs files for extra crate deps.
4. **`compile.rs`** — Generates the workspace (one lib crate per RUNME.rs + a runner binary crate), runs `cargo build`, returns the binary path.
5. **`crate_name.rs`** — Derives valid Rust crate names from file paths.

The generated runner crate calls `__runme_link()` on each lib crate to ensure `inventory` registrations aren't dropped by the linker.

### Task System (runme lib)

Tasks are registered at compile time via `inventory`. The `#[runme::task]` macro generates a `TaskDef` with name, description (from doc comments or `desc` attr), group, watch pattern, dependencies, and a wrapped function pointer.

- **`TaskContext`** — Runtime context passed to task functions. Provides `exec()` (run-and-wait) and `spawn()` (background process with handle). All child processes are spawned in their own process group for clean signal delivery.
- **`Registry`** — Collects `TaskDef`s from inventory, provides lookup and execution.
- **`Cmd`** — Value type describing a command. Two modes: structured (`Cmd::new("cargo").args(["build"])`) or shell (`Cmd::shell("echo hi")`). `&str` auto-converts to shell mode.

### Log Engine

Process output flows through a parsing pipeline: raw bytes → `RecordParser` → `RawRecord` → `FieldExtractor` → `LogEntry`. Parsers are tried in chain order (JSON, logfmt, cargo diagnostics, rust panics, plain text). The `OutputBuffer` is a ring buffer with broadcast notifications.

Key modules in `crates/runme/src/log/`:

- `parse/` — Record parsers (json, logfmt, cargo_diag, rust_panic, plain)
- `extract.rs` — Field extraction from parsed records (timestamp, level, message)
- `buffer.rs` — Ring buffer (`OutputBuffer`) with `tokio::broadcast` subscriber support
- `store.rs` — `LogStore` aggregates entries from multiple sources
- `filter.rs` / `search.rs` / `stream.rs` — Filtering, search, and streaming APIs

### TUI

Ratatui-based terminal UI in `crates/runme/src/tui/`. Modules:

- `app.rs` — Main app state machine and input handling
- `runner.rs` — `TaskRunner` manages task lifecycle and process tracking
- `render.rs` — Rendering logic for log viewer
- `viewport.rs` — Scroll state with vim-style navigation
- `sidebar.rs` — Source sidebar for filtering
- `event.rs` — Event loop (terminal events + app ticks)

### RUNME.rs File Convention

Every RUNME.rs file must define `const __RUNME_GROUP: &str = "...";` — this is injected by the code generator during compilation. For standalone tests/examples, define it manually as `""`.

Task functions are `async fn(ctx: &TaskContext) -> TaskResult`. The prelude re-exports tracing macros (`info!`, `error!`, etc.) for structured logging from task code.

## Rust Edition

This project uses Rust **edition 2024**. This means `use` imports within `impl` blocks, `unsafe_op_in_unsafe_fn` lint, and other 2024 edition changes apply. The nightly toolchain may be required.
