# runme

A task runner where tasks are real code. Define tasks in `RUNME.rs` files — plain Rust — and run them from anywhere in your directory tree.

The crate is published as `rnme`; the installed binary is `runme`.

```rust,ignore
use rnme::prelude::*;

/// Build the project
#[rnme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo build --release").await?;
    Ok(())
}

/// Start the dev server
#[rnme::task]
async fn start(ctx: &TaskContext) -> TaskResult {
    info!("Starting dev server");
    let _server = ctx.spawn("cargo run --bin server").await?;
    // The handle owns the process lifetime — wait until cancelled so
    // the server runs as long as the task does.
    ctx.cancellation_signal().await;
    Ok(())
}
```

No YAML. No DSLs. No config files. Just Rust.

## Why

I built this for me, based on my preferences. I'm sharing it because why not. It might not be to your taste. That's fine. Thanks for stopping by.

- **Code, not config.** All configuration tools eventually evolve to be turing complete. Any 'real' programming language, even JavaScript, is better than describing logic in YAML.
- **Useful dev tools** directly paired with the execution environment. The TUI and MCP interfaces offer rich interfaces for tasks you run a thousand times a day: watch files, restart, explore logs, dump data, perform complex workflows.
- **Agent friendly.** Stable command surface (`runme build`, `runme test`) means AI agents get consistent interfaces, structured output, and lower permission friction. Can also be used as an MCP for direct agent access via `runme --mcp`.
- **Works where you work.** `cargo` is a phenominal tool, but it demands a specific project layout that's not super nice for dev tooling that lives along side your application code. RUNME.rs files live anywhere in your directory tree — inside projects, above projects, across repos.

## A few things this is not

- **Not a build system** This is not meant to replace cargo, make, maven, whatever. There's currently no in-built dependency graph. This is meant to wrap those tools (and whatever else) to give you a useful surface for common tasks without needing to remember all the complex incantations.
- **Not a DSL** Just plain Rust code. I _loved_ [`rake`](https://github.com/ruby/rake), and [`cap`](https://github.com/capistrano/capistrano), but gymnastics required to make the DSL nice also made them impossible to reason about for more complex tasks.

## Project Status

This is a hobby project. I made it for myself. It's feature incomplete and the API is still changing rapidly, but it's far enough along to be useful.

## Install

```bash
cargo install rnme
```

## Usage

### Run a task

```bash
runme build
runme deploy --env staging
```

### List available tasks

```bash
runme list
```

### TUI mode

```bash
runme          # launches task picker, then TUI
runme build    # runs task directly in TUI
```

When launched with no arguments, `runme` opens a **task picker** showing all tasks grouped by their source RUNME.rs file. Type to fuzzy-filter across task names, descriptions, and group names. Press Enter to launch the selected task and transition to the log viewer.

## How It Works

`runme` discovers all RUNME.rs files in your directory tree, compiles them into a single binary, and runs it. The compilation pipeline:

1. **Discover** — Walk up from `cwd` to find the nearest RUNME.rs, then walk down to find children. `.gitignore` is respected.
2. **Generate** — Create a Cargo workspace in a cache directory. Each RUNME.rs becomes a library crate. A runner binary crate ties them together.
3. **Build** — `cargo build` with incremental compilation. The cache directory is stable per project, so rebuilds are fast.
4. **Exec** — Replace the `runme` process with the compiled binary.

Everything runs in-process: the TUI, log engine, process management, and all your tasks — no cross-process serialization.

## Examples

[`docs/examples/RUNME.rs`](docs/examples/RUNME.rs) is a runnable RUNME.rs file that exercises every feature shown below — watches, the `cmd!` macro, readiness probes, cross-task invocation, structured logging, task arguments, and more. It's the fastest way to see the API in one place.

## Writing Tasks

Tasks are async functions annotated with `#[rnme::task]`:

```rust,ignore
use rnme::prelude::*;

/// Run database migrations
#[rnme::task]
async fn migrate(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo run --bin migrate").await?;
    Ok(())
}
```

### Task descriptions

Doc comments become task descriptions, visible in `runme list` and the TUI:

```rust,ignore
/// Deploy to the specified environment
#[rnme::task]
async fn deploy(ctx: &TaskContext) -> TaskResult { ... }
```

### Task arguments

Tasks support progressive argument complexity:

```rust,ignore
// Simple args — extra params become CLI flags
#[rnme::task]
async fn deploy(ctx: &TaskContext, env: String, port: u16, verbose: bool) -> TaskResult {
    // runme deploy --env staging --port 8080 --verbose
    ...
}

// Full control — use a clap::Parser struct
#[derive(clap::Parser)]
struct DeployArgs {
    #[arg(short, long)]
    env: String,
    #[arg(short, long, default_value = "8080")]
    port: u16,
}

#[rnme::task]
async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult { ... }
```

### Commands

Three ways to describe a command:

```rust,ignore
// Shell string (pipes, globs, redirects).
ctx.exec("cargo build && cargo test").await?;

// Structured builder (no shell, no escaping issues).
// .cwd is resolved against the task's RUNME.rs directory (see below),
// so "./crates/server" works regardless of where rnme was launched from.
let cmd = Cmd::new("cargo")
    .args(["build", "--release"])
    .env("RUSTFLAGS", "-C target-cpu=native")
    .cwd("./crates/server");
ctx.exec(cmd).await?;

// cmd! macro — structured args with Rust-expression interpolation.
let url = format!("http://localhost:{port}/deploy");
ctx.exec(cmd![curl
                -X POST {&url}
                -H "Content-Type: application/json"
        ]).await?;
```

In the `cmd!` macro, whitespace separates arguments, `{expr}` interpolates a single argument, and `"..."` literals stay as one argument. No shell is invoked.

### Working directory

Every task runs with its cwd defaulting to the directory of the RUNME.rs that defines it — independent of where `rnme` was launched from or which other task invoked it. `.cwd(relative)` on a `Cmd` is joined under that directory; `.cwd(absolute)` is used verbatim. Task code that needs the path can read it via `ctx.task_dir()`.

### Background processes

`exec()` runs and waits. `spawn()` returns a handle for processes that should outlive the call. Add readiness probes and timeouts on the builder:

```rust,ignore
use std::time::Duration;

// Wait until the server is actually listening before continuing.
let server = ctx.spawn("./bin/api")
    .ready_on_port(8080)
    .ready_timeout(Duration::from_secs(30))
    .await?;

// Or probe via HTTP, or with a custom async closure.
let worker = ctx.spawn("./bin/worker")
    .ready_on_http("http://127.0.0.1:9000/healthz")
    .timeout(Duration::from_secs(60 * 60))   // hard kill after 1h
    .await?;

// handle.stop(timeout), handle.is_running(), handle.wait().await,
// handle.signal(sig), handle.wait_ready().await
```

When a task spawns dependent services, `ctx.bind_ready(&handle)` ties the task's own readiness to the process's probe — useful for orchestration tasks that other tasks depend on.

### Watching files

`ctx.watch()` returns a debounced stream of changed paths. The classic pattern is "do work, wait for changes, repeat":

```rust,ignore
/// Rebuild on every Rust source change.
#[rnme::task]
async fn dev(ctx: &TaskContext) -> TaskResult {
    let mut w = ctx.watch("src/**/*.rs").label("rust sources");
    loop {
        let result = ctx.exec("cargo build").await?;
        if !result.success() {
            error!("build failed (exit {})", result.exit_code());
        }
        w.next().await;   // blocks until something changes
    }
}
```

For more control, `ctx.watch_with()` takes a filter/map closure that runs on each batch, and `ctx.watch_channel()` returns a manual sender for non-filesystem triggers (timers, health checks, external events).

### Calling other tasks

Tasks call each other directly as typed Rust functions. `#[rnme::task]` rewrites each task into a shim that returns a `TaskBuilder` — the call looks like an ordinary fn call, but the body runs as a real child task with its own id, logs, cancellation, and ready state.

```rust,ignore
/// Full release: build, test, deploy.
#[rnme::task]
async fn release(ctx: &TaskContext) -> TaskResult {
    build(ctx).await?;                                                       // same file
    test(ctx).await?;
    subtasks::services::auth::deploy(ctx, "prod".to_string(), true).await?;  // cross-file
    Ok(())
}
```

For each parent RUNME.rs, the build system auto-injects a `mod subtasks` mirroring the directory layout of its descendants. Calls from a parent into a child use `subtasks::path::to::child::task_fn(...)`; calls inside the same file are just the task name. The builder is `#[must_use]`, so forgetting `.await?` or `.spawn()?` is a compile-time warning, not a silent no-op.

Two siblings whose directory names normalize to the same Rust identifier (e.g. `foo-bar/` and `foo_bar/`) collide inside `subtasks::parent::`. The build fails with a `SiblingNameCollision` error and prints the exact frontmatter snippet to paste into one of the children:

```rust,ignore
//! [rnme.rename]
//! name = "foo_bar_dashed"
```

For glob-driven fan-out, discovery, or sibling-to-sibling calls (which the typed path doesn't reach), the string-keyed dynamic path is still available:

```rust,ignore
ctx.run("services/auth:deploy", &["--env", "prod"]).await?;

if let Some(query) = ctx.tasks() {
    for task in query.matching("*:test") {
        ctx.run(&task.qualified_name, &[]).await?;
    }
}
```

`ctx.tasks()` returns a query handle for discovery — `.all()` lists every registered task, `.matching("services/*:deploy")` filters by glob.

### Per-file initialization

Override the default group name or perform setup with `#[rnme::init]`:

```rust,ignore
#[rnme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("Auth Service");
}
```

### Extra dependencies

Declare additional crate dependencies in frontmatter comments:

```rust,ignore
//! [dependencies]
//! reqwest = "0.12"
//! serde = { version = "1", features = ["derive"] }
```

## Directory Tree

RUNME.rs files form a hierarchy:

```text
~/Code/
  RUNME.rs              ← cross-repo orchestration
  services/
    RUNME.rs            ← service-level tasks
    auth/
      RUNME.rs          ← auth-specific tasks
  web-app/
    RUNME.rs            ← frontend tasks
```

Running `runme` from any directory discovers the full tree and makes all tasks available. Tasks are grouped by their source file.

## Log Engine

Process output is automatically parsed into structured log entries. JSON logs, logfmt, cargo diagnostics, and Rust panics are detected and parsed — fields like level, timestamp, and message are extracted for filtering and display.

Task code uses `tracing` macros for structured logging:

```rust,ignore
info!("deployment started");
error!(service = "auth", "connection failed");
```

These flow through the same pipeline as child process output.

## TUI Keyboard Shortcuts

### Task Picker

| Key          | Action                                            |
| ------------ | ------------------------------------------------- |
| typing       | Fuzzy filter tasks by name, description, or group |
| `j` / `Down` | Move selection down                               |
| `k` / `Up`   | Move selection up                                 |
| `Enter`      | Launch selected task                              |
| `Backspace`  | Delete last character of filter                   |
| `Ctrl-u`     | Clear filter input                                |
| `Esc` / `q`  | Quit                                              |

### Global

| Key      | Action                                      |
| -------- | ------------------------------------------- |
| `q`      | Quit                                        |
| `Ctrl-c` | Quit                                        |
| `Tab`    | Toggle focus between sidebar and log viewer |
| `?`      | Toggle help overlay                         |
| `\`      | Toggle sidebar visibility                   |

### Log Viewer

| Key                         | Action                          |
| --------------------------- | ------------------------------- |
| `j` / `Down`                | Next entry                      |
| `k` / `Up`                  | Previous entry                  |
| `Ctrl-d` / `]` / `PageDown` | Scroll down half page           |
| `Ctrl-u` / `[` / `PageUp`   | Scroll up half page             |
| `g` / `Home`                | Jump to first entry             |
| `G` / `End`                 | Jump to last entry (tail mode)  |
| `v` / `m`                   | Toggle preview/raw display mode |
| `w`                         | Toggle line wrap/truncate       |
| `f`                         | Open filter bar                 |
| `/`                         | Open search                     |
| `Enter`                     | Open entry detail view          |
| `n` / `N`                   | Next / previous search match    |
| `a`                         | Show all sources (clear filter) |
| `1`-`9`                     | Toggle visibility of source N   |
| `e`                         | Export visible log to file      |

### Entry Detail

| Key         | Action                                |
| ----------- | ------------------------------------- |
| `j` / `k`   | Scroll within detail                  |
| `Esc` / `q` | Close detail view                     |
| `n` / `N`   | Close and jump to next/previous entry |
| `y`         | Copy raw entry to clipboard (OSC 52)  |

### Filter / Search Input

| Key              | Action                                           |
| ---------------- | ------------------------------------------------ |
| typing           | Updates filter expression or search pattern      |
| `Enter`          | Confirm and return to normal mode                |
| `Esc`            | Cancel (revert) and return to normal mode        |
| `Ctrl-u`         | Clear input                                      |
| `Left` / `Right` | Move cursor within input                         |
| `Up` / `Down`    | Cycle through filter history (filter input only) |

### Sidebar

| Key          | Action                                                   |
| ------------ | -------------------------------------------------------- |
| `j` / `Down` | Move selection down                                      |
| `k` / `Up`   | Move selection up                                        |
| `Enter`      | Open process detail (process) / toggle visibility (task) |
| `Space`      | Toggle source visibility                                 |
| `s`          | Stop selected process (SIGTERM)                          |
| `S`          | Send SIGHUP to selected process                          |
| `a`          | Show all sources                                         |
| `1`-`9`      | Toggle visibility of source N                            |

### Process Detail

| Key         | Action                 |
| ----------- | ---------------------- |
| `j` / `k`   | Scroll within detail   |
| `s`         | Stop process (SIGTERM) |
| `S`         | Send SIGHUP            |
| `Esc` / `q` | Close detail view      |
