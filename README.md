# runme

A task runner where tasks are real code. Define tasks in `RUNME.rs` files — plain Rust with a shebang — and run them from anywhere in your directory tree.

```rust
#!/usr/bin/env runme

use runme::prelude::*;

/// Build the project
#[runme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo build --release").await?;
    Ok(())
}

/// Start the dev server
#[runme::task]
async fn start(ctx: &TaskContext) -> TaskResult {
    info!("Starting dev server");
    let handle = ctx.spawn("cargo run --bin server").await?;
    Ok(())
}
```

No YAML. No DSLs. No config files. Just Rust.

## Why

- **Code, not config.** Conditional logic, dependency graphs, error handling — use a real language instead of pretending YAML can do it.
- **Executable documentation.** The RUNME.rs file _is_ the documentation. It runs.
- **AI-friendly.** Stable command surface (`runme build`, `runme test`) means AI agents get consistent interfaces, structured output, and lower permission friction.
- **Works where you work.** RUNME.rs files live anywhere in your directory tree — inside projects, above projects, across repos. The tool assembles them automatically.

## Install

```bash
cargo install --path crates/runme-cli
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
runme          # launches interactive TUI
runme --tui    # explicit TUI mode
```

### Direct execution

RUNME.rs files are directly executable:

```bash
chmod +x RUNME.rs
./RUNME.rs build
```

## How It Works

`runme` discovers all RUNME.rs files in your directory tree, compiles them into a single binary, and runs it. The compilation pipeline:

1. **Discover** — Walk up from `cwd` to find the nearest RUNME.rs, then walk down to find children. `.gitignore` is respected.
2. **Generate** — Create a Cargo workspace in a cache directory. Each RUNME.rs becomes a library crate. A runner binary crate ties them together.
3. **Build** — `cargo build` with incremental compilation. The cache directory is stable per project, so rebuilds are fast.
4. **Exec** — Replace the `runme` process with the compiled binary.

Everything runs in-process: the TUI, log engine, process management, and all your tasks — no cross-process serialization.

## Writing Tasks

Tasks are async functions annotated with `#[runme::task]`:

```rust
#!/usr/bin/env runme

use runme::prelude::*;

/// Run database migrations
#[runme::task]
async fn migrate(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo run --bin migrate").await?;
    Ok(())
}
```

### Task descriptions

Doc comments become task descriptions, visible in `runme list` and the TUI:

```rust
/// Deploy to the specified environment
#[runme::task]
async fn deploy(ctx: &TaskContext) -> TaskResult { ... }
```

### Task arguments

Tasks support progressive argument complexity:

```rust
// Simple args — extra params become CLI flags
#[runme::task]
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

#[runme::task]
async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult { ... }
```

### Commands

Two ways to run commands:

```rust
// Shell string (pipes, globs, redirects)
ctx.exec("cargo build && cargo test").await?;

// Structured command (no shell, no escaping issues)
let cmd = Cmd::new("cargo")
    .args(["build", "--release"])
    .env("RUSTFLAGS", "-C target-cpu=native")
    .cwd("./crates/server");
ctx.exec(cmd).await?;
```

`exec()` runs and waits. `spawn()` starts a background process and returns a handle:

```rust
let mut handle = ctx.spawn("npm run dev").await?;
// handle.stop(), handle.is_running(), handle.wait()
```

### Per-file initialization

Override the default group name or perform setup with `#[runme::init]`:

```rust
#[runme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("Auth Service");
}
```

### Extra dependencies

Declare additional crate dependencies in frontmatter comments:

```rust
//! [dependencies]
//! reqwest = "0.12"
//! serde = { version = "1", features = ["derive"] }
```

## Directory Tree

RUNME.rs files form a hierarchy:

```
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

```rust
info!("deployment started");
error!(service = "auth", "connection failed");
```

These flow through the same pipeline as child process output.

## TUI Keyboard Shortcuts

### Global

| Key | Action |
|-----|--------|
| `q` | Quit |
| `Ctrl-c` | Quit |
| `Tab` | Toggle focus between sidebar and log viewer |
| `?` | Toggle help overlay |

### Log Viewer

| Key | Action |
|-----|--------|
| `j` / `Down` | Next entry |
| `k` / `Up` | Previous entry |
| `Ctrl-d` / `]` / `PageDown` | Scroll down half page |
| `Ctrl-u` / `[` / `PageUp` | Scroll up half page |
| `g` / `Home` | Jump to first entry |
| `G` / `End` | Jump to last entry (tail mode) |
| `v` / `m` | Toggle preview/raw display mode |
| `w` | Toggle line wrap/truncate |
| `f` | Open filter bar |
| `/` | Open search |
| `n` / `N` | Next / previous search match |
| `a` | Show all sources (clear filter) |
| `1`-`9` | Toggle visibility of source N |

### Filter / Search Input

| Key | Action |
|-----|--------|
| typing | Updates filter expression or search pattern |
| `Enter` | Confirm and return to normal mode |
| `Esc` | Cancel (revert) and return to normal mode |
| `Ctrl-u` | Clear input |
| `Left` / `Right` | Move cursor within input |

### Sidebar

| Key | Action |
|-----|--------|
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `Enter` / `Space` | Toggle source visibility |
| `a` | Show all sources |
| `1`-`9` | Toggle visibility of source N |
