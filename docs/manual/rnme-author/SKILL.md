---
name: rnme-author
description: Use this skill when extending or writing tasks in a project that uses rnme — adding a build pipeline, dev-server task, deploy task, or any other automation defined in a RUNME.rs file. Covers RUNME.rs file structure, the #[rnme::task] / #[rnme::init] macros, the cmd! macro, ctx.exec / ctx.spawn, readiness probes, timeouts, task arguments (zero-arg, simple flags, clap structs), tracing macros, ctx.summary, frontmatter [dependencies], and the discovery and group conventions for placing RUNME.rs files in a project tree.
---

# Authoring RUNME.rs

A `RUNME.rs` file is a plain Rust source file. Tasks are `async fn` items annotated with `#[rnme::task]`. There is no YAML, no DSL, and no `Cargo.toml` editing for normal use.

If you only need to *run* tasks, see the `rnme` skill instead.

## File anatomy

Every RUNME.rs starts the same way:

```rust
use rnme::prelude::*;
```

The prelude exposes the macros and types you'll use: `#[rnme::task]`, `#[rnme::init]`, `cmd!`, `Cmd`, `TaskContext`, `TaskResult`, `SpawnOptions`, plus the tracing macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

A minimal task:

```rust
use rnme::prelude::*;

/// Build the project.
#[rnme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo build").await?;
    Ok(())
}
```

The doc comment (`///`) becomes the task's description in `list_tasks` output. **Always write one.** It's how operators (humans and agents) know what your task does without reading the code. Do **not** use a `desc = "..."` attribute — descriptions come exclusively from doc comments.

## Discovery and groups

- rnme finds the nearest ancestor RUNME.rs from the user's cwd, then walks down to find children. `.gitignore` is respected.
- Each RUNME.rs file is a "group". The root file's tasks have empty group prefix — `qualified_name = "build"`. A file at `services/api/RUNME.rs` produces tasks with `qualified_name = "services/api:build"`.
- Tasks call each other directly as Rust functions — same file, just `build(ctx).await?`; cross-file, `subtasks::services::api::build(ctx).await?`. The string-keyed `ctx.run("group:name", &[args])` path is still available for discovery-driven invocation. See *Cross-task invocation* below.
- Drop new RUNME.rs files anywhere in the tree. The discovery walker picks them up.

## Task argument forms

Three forms, in order of complexity. Use the simplest one that fits.

### 1. Zero-arg

```rust
#[rnme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo build").await?;
    Ok(())
}
```

Most tasks. No CLI args.

### 2. Simple typed parameters

Each parameter becomes a `--flag`. They are *not* positional.

```rust
/// Deploy to an environment.
///
/// Usage: rnme deploy --env staging --port 8080 --verbose
#[rnme::task]
async fn deploy(ctx: &TaskContext, env: String, port: u16, verbose: bool) -> TaskResult {
    info!("Deploying to {} on port {} (verbose: {})", env, port, verbose);
    Ok(())
}
```

Parameters are required by default. Use this form for simple, named knobs.

### 3. clap::Parser struct

When you need positional args, defaults, rich help, or any other clap feature, define a struct that derives `clap::Parser` and pass it as the second argument.

```rust
use rnme::clap;   // brings clap into scope so derives resolve

#[derive(clap::Parser)]
struct GreetArgs {
    /// Name to greet.
    #[arg(long)]
    name: String,
    /// Number of times to greet.
    #[arg(long, default_value = "1")]
    count: u32,
}

/// Greet someone.
#[rnme::task]
async fn greet(ctx: &TaskContext, args: GreetArgs) -> TaskResult {
    for i in 0..args.count {
        ctx.println(format!("[{}/{}] Hello, {}!", i + 1, args.count, args.name)).await;
    }
    Ok(())
}
```

The `use rnme::clap;` line is required for the derive macro path to resolve.

## Running commands

Two entry points:

- `ctx.exec(cmd)` — spawn, **wait for exit**, return `ProcessResult`. The basic "run this and don't proceed until it's done" call.
- `ctx.spawn(cmd)` — fire-and-forget. Returns a `SpawnBuilder`. `.await` yields a `ProcessHandle` that keeps the process alive while the handle is in scope. `.complete().await` waits for exit and yields a `ProcessResult`.

### Failing the task on a non-zero exit code

`ctx.exec` returns a `ProcessResult`. Two `?` markers do different things:

```rust
ctx.exec("cargo build").await?;          // fails the task only if spawn itself errored
ctx.exec("cargo build").await?.ok()?;    // ALSO fails the task on non-zero exit
```

`.ok()?` is the idiomatic shape for "this command must succeed". Use it on every `exec` where a non-zero exit means task failure.

### The `cmd!` macro

For commands more structured than a single string, use `cmd!`:

```rust
let url = "https://api.example.com/deploy";
ctx.exec(cmd!(curl -X POST {&url} -H "Content-Type: application/json")).await?.ok()?;
```

Rules:
- Whitespace separates args (no shell parsing).
- `{expr}` interpolates a Rust expression as a single arg.
- `"..."` is a single literal arg (preserves spaces inside).
- **No shell is invoked.** No quoting headaches, no `$VAR` expansion, no `&&` chaining.

Use `cmd!` whenever the command is structurally simple. Use `Cmd::from("bash -c '...'")` only when you genuinely need a shell (pipes, redirections, `&&` chains). Bash one-liners with embedded quotes are error-prone — prefer `cmd!` plus a separate `bash -c` only when unavoidable.

The `Cmd` builder also supports `.env(k, v)` and `.cwd(path)`:

```rust
let cmd = cmd!(env)
    .env("RNME_DEMO", "hello")
    .env("RNME_MODE", "testing");
ctx.exec(cmd).await?;
```

## Working directory

Every task runs with its cwd defaulting to the directory of the RUNME.rs that defines it. Write your tasks as if you were standing in that directory — `./scripts/foo`, `target/release/bin`, `client_web/Cargo.toml` all resolve the way you'd expect, regardless of where the user invoked `rnme` from.

```rust
// In services/api/RUNME.rs — these always work.
ctx.exec(cmd!(cargo build --release)).await?.ok()?;
ctx.exec("./scripts/deploy.sh").await?.ok()?;
```

`.cwd(...)` on a `Cmd` is resolved against the task's directory too:

| `.cwd(...)` argument | Effective cwd                          |
|----------------------|----------------------------------------|
| omitted              | `<task dir>`                           |
| relative path        | `<task dir>/<path>` (joined)           |
| absolute path        | the absolute path verbatim             |

```rust
// In monorepo/RUNME.rs — runs `wasm-pack` in monorepo/client_web/.
let action = cmd![wasm-pack build --target web -d ./target/wasmpack --no-typescript]
    .cwd("client_web");
ctx.exec(action).await?.ok()?;
```

For Rust code in the task body that needs to read files relative to the RUNME.rs, use `ctx.task_dir() -> Option<&Path>`. Join from there — don't rely on `std::env::current_dir()`, which reflects the rnme process's launch directory, not the task's.

## Long-running processes

For a service that should die when the task ends:

```rust
#[rnme::task]
async fn dev_server(ctx: &TaskContext) -> TaskResult {
    let _server = ctx.spawn("./bin/api").await?;
    // _server lives until the function returns or is cancelled.
    // Wait for cancellation so the process keeps running until killed.
    ctx.cancellation_signal().await;
    Ok(())
}
```

The `_server` handle's drop sends signals to the process group. When the task is cancelled (`kill_task`), the body's `cancellation_signal` future resolves, the function returns, and the handle drops cleanly.

For multiple processes:

```rust
let _fast = ctx.spawn(Cmd::from("./bin/fast").label("fast")).await?;
let _slow = ctx.spawn(Cmd::from("./bin/slow").label("slow")).await?;
ctx.cancellation_signal().await;
Ok(())
```

`.label("...")` gives the process a human-readable name in graphs and logs.

## Readiness probes

When a process is "ready" only after some condition (a server listening on a port, a build watch loop established), declare the probe on the `SpawnBuilder`:

```rust
use std::time::Duration;

let server = ctx.spawn("python3 -m http.server 8765")
    .ready_on_port(8765)                         // wait until something listens on :8765
    .ready_timeout(Duration::from_secs(10))      // bound the probe
    .timeout(Duration::from_secs(60))            // hard-kill the process after 60s
    .await?;
```

Three probe modes:

- `.ready_on_port(port)` — TCP-connect probe. Ready when a connection opens.
- `.ready_on_http(url)` — HTTP polling. Ready when the URL returns 2xx.
- `.ready_when(closure)` — custom predicate, called repeatedly until it returns `true`.

Tying the probe to task status:

```rust
let server = ctx.spawn("./bin/api").ready_on_port(8080).await?;
ctx.bind_ready(&server);   // task status flips to Ready when the process is ready
```

`mark_ready()` flips the task to Ready directly (use when readiness is computed in task code, not by a probe).

## Timeouts

- `.timeout(Duration)` on the `SpawnBuilder` — hard-kills the process after the duration.
- `.ready_timeout(Duration)` on the `SpawnBuilder` — bounds only the readiness probe.
- The MCP-side `timeout_seconds` on `spawn_task` / `run_task` — caps the entire task body.

## Tracing and structured output

Use the tracing macros for anything you'd want to surface in a task report:

```rust
info!("starting build");
warn!("cache miss, falling back to full rebuild");
error!("compile failed: {}", err);
debug!(target = ?path, "writing artifact");
```

These appear in the task report's `Events` line and in `get_logs` as structured entries.

For raw, undecorated output (no level prefix, no formatting):

```rust
ctx.println("plain text output").await;
```

## `ctx.summary` — speak to operators

When a task wraps up, you can leave a one-line (or short multi-line) summary that displaces the log tail in the rendered report:

```rust
#[rnme::task]
async fn build(ctx: &TaskContext) -> TaskResult {
    let result = ctx.exec("cargo build --message-format=json").await?;
    if !result.success() {
        ctx.summary(format!("Build failed: {} errors. See log tail.", count_errors));
        return Err(result.into());
    }
    ctx.summary(format!("Build succeeded. {} warnings. Output: target/debug/myapp", warning_count));
    Ok(())
}
```

Last-write-wins. The summary appears in the `Summary:` block of `get_task` / `run_task` output and replaces the log tail. Use it whenever your task can communicate "what happened" succinctly — it's the agent-facing equivalent of a build status message.

## Cross-task invocation

There are two paths. Prefer the typed path; reach for the dynamic path when you need it.

### Typed path (default)

`#[rnme::task]` rewrites each task into a thin shim that returns a `TaskBuilder`. Calling the task by name *looks* like an ordinary function call, but it doesn't run the body inline — it constructs a builder that the engine drives, so the call is a real child task (own `TaskId`, own log source, own cancellation/ready state, own optional timeout).

```rust
// Same-file: just the task's name.
build(ctx).await?;
deploy(ctx, "staging".to_string(), 8080, false).await?;        // simple primitives
greet(ctx, GreetArgs { name: "world".into(), count: 3 }).await?;  // struct arg

// Cross-file: prefixed with the subtasks tree, mirroring the directory layout.
subtasks::services::api::build(ctx).await?;
subtasks::services::api::deploy(ctx, "prod".to_string(), 443, true).await?;
```

Three call shapes, matching the three argument forms:
- 0-arg tasks: `build(ctx).await?`
- Simple-primitive tasks: `deploy(ctx, "staging".to_string(), 8080, false).await?` (positional, in declaration order)
- Struct-arg tasks: `greet(ctx, GreetArgs { ... }).await?`

Both `.await?` and `.spawn()?` work on the returned builder. `.spawn()?` returns a `TaskHandle` for fire-and-forget; `.await?` waits for completion.

The builder is `#[must_use]`: writing `build(ctx);` without `.await?` or `.spawn()?` is a compiler warning, because that pattern silently constructs and drops the builder without running anything.

**Sibling-direction calls don't work.** If `services/api/RUNME.rs` wants to call `services/worker`'s tasks, neither file is in the other's descendant set, so the typed path won't compile. Either orchestrate from the common parent, or factor the shared logic into a regular lib crate. Or fall back to the dynamic path.

### The `subtasks::` tree

For each parent RUNME.rs, the build system auto-injects `mod subtasks` mirroring the directory layout of its descendants. Given:

```
services/RUNME.rs
services/api/RUNME.rs
services/api/worker/RUNME.rs
services/shared/db/RUNME.rs       (services/shared/ has no RUNME.rs)
```

`services/RUNME.rs` sees:

```rust
subtasks::api::*                      // tasks + pub items from services/api/RUNME.rs
subtasks::api::worker::*              // ditto for services/api/worker/RUNME.rs
subtasks::shared::db::*               // structural shared/ + real db/
```

Two practical consequences:

1. **All `pub` items in a child RUNME.rs propagate up.** Task fns are `pub fn`; so are any `pub struct` (e.g. clap arg structs) or `pub fn` helpers the child declares. This is how struct-arg tasks work cross-file — the parent needs to name the type. Be deliberate about what you mark `pub` in a RUNME.rs.
2. **Adding or removing a middle-tier RUNME.rs doesn't break call paths.** Dropping `services/shared/RUNME.rs` in keeps `subtasks::shared::db::...` resolving exactly as before.

### `[rnme.rename]` — collision escape hatch

Two sibling directories whose names normalize to the same Rust identifier (e.g. `foo-bar/` next to `foo_bar/`, or `Foo/` next to `foo/`) would clash inside `subtasks::parent::`. The build fails with a `SiblingNameCollision` error that names both paths and prints the exact snippet to paste into one of them.

Rename frontmatter goes at the top of the child RUNME.rs:

```rust
//! [rnme.rename]
//! name = "foo_bar_dashed"
```

The replacement string is substituted for the directory name *before* normalization, so `"foo_bar_dashed"` produces the identifier `foo_bar_dashed`; `"Hello World"` would produce `hello_world`. Rename is available for any purpose (clarity, branding, decoupling the exposed name from the on-disk name), not just collision resolution.

### Dynamic path

The string-keyed `ctx.run(name, args)` remains the right tool for:

- Glob-driven fan-out where the name isn't statically known.
- Sibling-to-sibling calls (typed path doesn't reach across siblings).
- Forwarding string args received from the CLI / MCP without retyping them.

```rust
ctx.run("services/api:build", &[]).await?;
ctx.run("deploy", &["--env", "staging"]).await?;

if let Some(query) = ctx.tasks() {
    for task in query.matching("*:test") {
        info!("Running {}", task.qualified_name);
        ctx.run(&task.qualified_name, &[]).await?;
    }
}
```

Both paths converge on the same engine machinery and produce the same `TaskHandle`. The dynamic path stringifies args and re-parses them through the callee's clap parser; the typed path skips that round-trip.

`ctx.tasks()` returns `Option<TaskQuery>` — `None` only when running outside the full registry (rare; defensively guard if you care).

## Frontmatter `[dependencies]`

Need crates beyond what the prelude provides? Declare them at the top of the file:

```rust
//! [dependencies]
//! reqwest = { version = "0.11", features = ["json"] }
//! anyhow = "1"
//! serde = { version = "1", features = ["derive"] }

use rnme::prelude::*;
use serde::Deserialize;
// ...
```

These become Cargo dependencies of the generated workspace crate. No `Cargo.toml` editing.

## Per-file init

```rust
#[rnme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("api");          // human-readable group label
    // ctx.register_task(...)            // dynamic task registration (rare)
}
```

Runs once when the registry is built. Common uses:

- `set_group_name(...)` — display-only; the qualified name is still derived from the file path.
- `register_task(...)` — register tasks at runtime when their definition can't be a static `#[rnme::task]` (e.g. one task per discovered config file). Most projects don't need this.

## Common patterns

### Long-running service that dies on cancellation

```rust
#[rnme::task]
async fn dev(ctx: &TaskContext) -> TaskResult {
    let _server = ctx.spawn("./bin/api").ready_on_port(8080).await?;
    ctx.cancellation_signal().await;
    Ok(())
}
```

### Sequential pipeline

```rust
#[rnme::task]
async fn check(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo fmt --check").await?.ok()?;
    ctx.exec("cargo clippy -- -D warnings").await?.ok()?;
    ctx.exec("cargo test").await?.ok()?;
    ctx.summary("All checks passed.");
    Ok(())
}
```

### Multi-phase pipeline with labeled steps

```rust
#[rnme::task]
async fn release(ctx: &TaskContext) -> TaskResult {
    {
        let _step = ctx.begin_step("compile");
        ctx.exec("cargo build --release").await?.ok()?;
    }
    {
        let _step = ctx.begin_step("test");
        ctx.exec("cargo test --release").await?.ok()?;
    }
    {
        let _step = ctx.begin_step("package");
        ctx.exec("./scripts/package.sh").await?.ok()?;
    }
    ctx.summary("Release pipeline complete.");
    Ok(())
}
```

`begin_step` returns an RAII guard. Drop completes the step; `step.fail()` marks it failed.

### Watch-and-rebuild

```rust
#[rnme::task]
async fn watch_build(ctx: &TaskContext) -> TaskResult {
    let mut w = ctx.watch("**/*.rs").label("rust sources");
    loop {
        info!("Building...");
        let result = ctx.exec("cargo build").await?;
        if result.success() {
            info!("Build succeeded");
        } else {
            error!("Build failed (exit {})", result.exit_code());
        }
        w.next().await;   // block until a watched file changes
    }
}
```

### Concurrent tasks

```rust
#[rnme::task]
async fn parallel(ctx: &TaskContext) -> TaskResult {
    let (a, b) = tokio::join!(build(ctx), docs(ctx));
    a?;
    b?;
    Ok(())
}
```

Each typed call materializes as a separate child task in the engine graph, so the join above runs `build` and `docs` concurrently with independent log sources and status.

## Anti-patterns

- **Don't use `std::process::Command` directly.** The task runner won't track the process group, can't cancel it, and won't capture its output. Use `ctx.exec` / `ctx.spawn`.
- **Don't `tokio::spawn` task body work.** The engine manages the body's lifetime; spawned background work escapes cancellation. Keep work in the body's future, or use `ctx.spawn` for processes.
- **Don't build a manual wait loop around `ctx.exec`.** `.await?.ok()?` already blocks until exit and propagates failure. Polling on top of it is redundant.
- **Don't use `desc = "..."` on `#[rnme::task]`.** Descriptions come from doc comments only.
- **Be deliberate about `pub` in a RUNME.rs.** Each RUNME.rs is a lib crate, and a parent's auto-generated `mod subtasks` re-exports the full `pub` surface of every descendant. `pub fn`, `pub struct`, `pub use` items propagate up — which is exactly how struct-arg tasks work cross-file (`subtasks::child::WebOpts { ... }`), but it also means anything you accidentally mark `pub` will appear in the parent's namespace too.
- **Don't drop a builder without `.await?` or `.spawn()?`.** Both `ctx.spawn(...)` and the typed task shim (`build(ctx).await?`) return `#[must_use]` builders. Writing `build(ctx);` constructs a builder and drops it — nothing runs. The compiler warns; treat the warning as an error.
- **Don't shell out via `bash -c '...'` when `cmd!` works.** Quoting bugs are silent and dangerous; `cmd!` arg-splits cleanly.
- **Don't forget `.ok()?` on `exec` calls that must succeed.** A bare `.await?` only fails on spawn errors, not exit code.

## Reference: the prelude

`use rnme::prelude::*;` brings in:

- Macros: `#[rnme::task]`, `#[rnme::init]`, `cmd!`
- Types: `TaskContext`, `TaskResult`, `Cmd`, `SpawnBuilder`, `ProcessHandle`, `ProcessResult`, `Termination`, `InitContext`, `SpawnOptions`
- Tracing macros: `trace!`, `debug!`, `info!`, `warn!`, `error!`
- Helpers: `glob_filter` (for `watch_with` predicates)

For clap derives, additionally `use rnme::clap;`.
