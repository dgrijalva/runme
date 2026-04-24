# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Architecture

rnme is a Rust-based task runner where tasks are defined in `RUNME.rs` files — plain Rust source files. No YAML, no DSLs.

### Project Layout

- **`src/`** — Core library (`rnme` crate). Contains the task runtime, process management, log engine, TUI, and the prelude that RUNME.rs files import.
- **`src/bin/rnme/`** — The `rnme` binary. Handles discovery of RUNME.rs files, workspace generation, compilation, and exec of the resulting binary.
- **`macros/`** — Proc macros (`rnme-macros` crate): `#[rnme::task]`, `#[rnme::init]`, and `cmd!`. Generates `TaskDef`/`InitDef` registrations via `inventory`, and structured `Cmd` values from shell-like syntax.
- **`testing/test-tasks/`** — Test fixture crate (`rnme-test-tasks`) for cross-crate inventory linking tests.

The library and CLI binary are a single crate (`rnme`), so `cargo install rnme` gives you the `rnme` binary while `use rnme::prelude::*` gives you the library.

### Compilation Pipeline (src/bin/rnme/)

The CLI discovers all RUNME.rs files in a directory tree, generates a Cargo workspace in a cache directory, builds it, and execs the resulting binary. Key modules:

1. **`discover.rs`** — Walks up to find the nearest RUNME.rs, then walks down to find children. Respects `.gitignore` via the `ignore` crate.
2. **`transform.rs`** — Source transformation: strips frontmatter, injects `__RNME_GROUP` constant.
3. **`frontmatter.rs`** — Parses `//! [dependencies]` frontmatter from RUNME.rs files for extra crate deps.
4. **`compile.rs`** — Generates the workspace (one lib crate per RUNME.rs + a runner binary crate), runs `cargo build`, returns the binary path.
5. **`crate_name.rs`** — Derives valid Rust crate names from file paths.

The generated runner crate calls `__rnme_link()` on each lib crate to ensure `inventory` registrations aren't dropped by the linker.

### Task System (rnme lib)

Tasks are registered via `inventory` at compile time, or dynamically at init time. The `#[rnme::task]` macro generates a `TaskDef` with name, description (from doc comments or `desc` attr), group, and a wrapped function.

- **`TaskDef`** — Task metadata. The `func` field is `TaskFnKind`: either `Static(TaskFn)` (function pointer from `#[rnme::task]`, const-constructible for `inventory::submit!`) or `Dynamic(Arc<dyn Fn>)` (closure with captured state, from `InitContext::register_task()`).
- **`TaskContext`** — Runtime context passed to task functions. `spawn()` returns a `SpawnBuilder` (via `IntoFuture`, so `.await` works unchanged) with optional readiness conditions (`.ready_on_port()`, `.ready_on_http()`, `.ready_when()`), timeouts (`.timeout()`, `.ready_timeout()`), and `.complete()` for wait-for-exit. `exec()` is sugar for `spawn().complete()`. `bind_ready(&handle)` / `mark_ready()` wire process readiness to `TaskStatus::Ready`. All child processes are spawned in their own process group for clean signal delivery.
- **`Registry`** — Collects `TaskDef`s from inventory + dynamic registration, provides lookup and execution.
- **`InitContext`** — Passed to `#[rnme::init]` hooks. Can set group display name and register dynamic tasks via `register_task()`. Dynamic tasks have their strings leaked to `&'static str` (process-lifetime, bounded count).
- **`Cmd`** — Pure value type describing a command. Two modes: structured (`Cmd::new("cargo").args(["build"])`) or shell (`Cmd::shell("echo hi")`). `&str` auto-converts to shell mode. The `cmd!` macro provides shell-like syntax that compiles to structured args: `cmd!(curl -X POST {&url} -H "Content-Type: application/json")`. Whitespace separates args, `{expr}` interpolates, `"..."` is a single literal arg. No shell involved. Runtime behavior (timeout, readiness) lives on `SpawnBuilder`, not on `Cmd`.
- **`SpawnBuilder`** — Returned by `ctx.spawn()`. Supports `.ready_on_port()`, `.ready_on_http()`, `.ready_when()` for declarative readiness, `.timeout()` for process lifetime, `.ready_timeout()` for probe lifetime. `.await` (via `IntoFuture`) returns `ProcessHandle`; `.complete().await` returns `ProcessResult`.
- **`Termination`** — Enum on `ProcessResult`: `Exited(i32)`, `Signaled(Signal)`, `TimedOut`. Replaces bare exit codes with richer termination semantics.

### Log Engine

Process output flows through a parsing pipeline: raw bytes → `RecordParser` → `RawRecord` → `FieldExtractor` → `LogEntry`. Parsers are tried in chain order (JSON, logfmt, cargo diagnostics, rust panics, plain text). The `OutputBuffer` is a ring buffer with broadcast notifications.

Key modules in `src/log/`:

- `parse/` — Record parsers (json, logfmt, cargo_diag, rust_panic, plain)
- `extract.rs` — Field extraction from parsed records (timestamp, level, message)
- `buffer.rs` — Ring buffer (`OutputBuffer`) with `tokio::broadcast` subscriber support
- `store.rs` — `LogStore` aggregates entries from multiple sources. `output()` and `output_for(source)` produce `Output` handles with snapshot + live forwarding.
- `filter.rs` / `search.rs` / `stream.rs` — Filtering, search, and streaming APIs

### TUI

Ratatui-based terminal UI in `src/tui/`. Modules:

- `app.rs` — Main app state machine and input handling
- `runner.rs` — `TaskRunner` manages task lifecycle and process tracking
- `render.rs` — Rendering logic for log viewer
- `viewport.rs` — Scroll state with vim-style navigation
- `sidebar.rs` — Source sidebar for filtering
- `event.rs` — Event loop (terminal events + app ticks)

### RUNME.rs File Convention

Every RUNME.rs file must define `const __RNME_GROUP: &str = "...";` — this is injected by the code generator during compilation. For standalone tests/examples, define it manually as `""`.

Task functions are `async fn(ctx: &TaskContext) -> TaskResult`. The prelude re-exports tracing macros (`info!`, `error!`, etc.) for structured logging from task code.

## Project Status

This is pre-release, unshipped software. There are no external users. Do not worry about backward compatibility — APIs can change freely.

## Rust Edition

This project uses Rust **edition 2024**. This means `use` imports within `impl` blocks, `unsafe_op_in_unsafe_fn` lint, and other 2024 edition changes apply. The nightly toolchain may be required.
