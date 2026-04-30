# Build System Design

## Overview

The `rnme` CLI discovers all RUNME.rs files in a directory tree, compiles them into a single binary, and execs it. The binary contains every task from every file, the TUI, the log engine, and the process management runtime — all in-process, no cross-process serialization. The library and CLI binary are merged into one crate (`rnme`); `cargo install rnme` gives you the `rnme` binary, `use rnme::prelude::*` gives you the library.

## Pipeline

1. **Discover** all RUNME.rs files in the tree (`src/bin/rnme/discover.rs` — walk up to find root, walk down to find children, respect `.gitignore` via the `ignore` crate)
2. **Generate** the workspace in a cache directory (always — cheap to write a few files)
3. **`cargo build`** (always — let Cargo's incremental compilation decide whether to actually recompile)
4. **Exec** the resulting binary, passing through all arguments

## Cache Directory

Keyed by the absolute path of the root RUNME.rs file (the discovery root), not by content hash. The workspace for `/Users/me/Code/myproject/RUNME.rs` always lands in the same cache directory regardless of source changes. This gives Cargo a stable `target/` directory for incremental compilation.

Naming: hash the root path to get a stable, filesystem-safe directory name.

```
~/.cache/rnme/<hash-of-root-path>/
```

## Workspace Structure

```
~/.cache/rnme/<hash>/
├── Cargo.toml                 # workspace manifest
├── root/
│   ├── Cargo.toml             # lib crate for ./RUNME.rs
│   └── src/lib.rs             # transformed source
├── services_auth/
│   ├── Cargo.toml             # lib crate for services/auth/RUNME.rs
│   └── src/lib.rs
├── web_app/
│   ├── Cargo.toml             # lib crate for web-app/RUNME.rs
│   └── src/lib.rs
└── runner/
    ├── Cargo.toml             # bin crate — depends on all the above
    └── src/main.rs            # generated entry point
```

Each RUNME.rs file becomes a library crate; a runner binary crate depends on all of them and provides the entry point.

## Source Transformation

Each RUNME.rs source is transformed before being written as `src/lib.rs` in its generated crate (see `src/bin/rnme/transform.rs`):

1. **Strip frontmatter** — leading `//!` doc-comment lines (already parsed into Cargo.toml dependencies)
2. **Inject group constant** — a `const` at the top that the `#[task]` macro reads to populate `TaskDef.group`:
   ```rust
   const __RNME_GROUP: &str = "services/auth";
   ```
3. **Append a link symbol** — `pub fn __rnme_link() {}` so the runner crate can reference it. Without a referenced symbol, the linker would dead-strip the lib crate's object files and `inventory` registrations would silently disappear.

There is no `fn main()` in RUNME.rs files. The runner crate provides the entry point.

## RUNME.rs File Anatomy

A RUNME.rs file contains:

- Task definitions (`#[rnme::task]` annotated functions)
- An optional init hook (`#[rnme::init]`)
- Imports, helper functions, whatever Rust code the author needs

```rust
//! [dependencies]
//! reqwest = "0.12"

use rnme::prelude::*;

#[rnme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("Auth Service");
}

/// Run database migrations
#[rnme::task]
async fn migrate(ctx: &TaskContext) -> TaskResult {
    ctx.exec("cargo run --bin migrate").await?;
    Ok(())
}

/// Deploy to an environment
#[rnme::task]
async fn deploy(ctx: &TaskContext, env: String, dry_run: bool) -> TaskResult {
    if dry_run {
        ctx.println(format!("would deploy to {env}")).await;
    } else {
        ctx.exec(format!("deploy --target {env}")).await?;
    }
    Ok(())
}
```

## `#[rnme::init]` — Per-File Initialization

Optional. Registered via `inventory` like tasks.

```rust
pub struct InitDef {
    pub group: &'static str,     // default group (injected __RNME_GROUP)
    pub func: fn(&mut InitContext),
}
```

`InitContext` is pre-populated with the path-based group name. The init function can override it or configure other per-file settings. Scoped to the file's own configuration — no access to other files' tasks or groups.

```rust
pub struct InitContext {
    group_name: String,
    // future: custom parsers, shared state, etc.
}

impl InitContext {
    pub fn set_group_name(&mut self, name: &str) { ... }
    pub fn group_name(&self) -> &str { ... }
    pub fn register_task(&mut self, ...);  // dynamic task registration
}
```

**Init ordering**: leaf-to-root. Deepest files in the directory tree run first, root runs last. Siblings have no guaranteed ordering relative to each other. The root's init runs after all children have registered their tasks and groups.

Files without `#[rnme::init]` get the path-based group name automatically.

## Task Groups

Tasks are associated with groups, but group metadata is separate from `TaskDef`. Each RUNME.rs file produces a `GroupDef` (registered via `inventory` by the code generator):

```rust
pub struct GroupDef {
    pub key: &'static str,        // the __RNME_GROUP value (relative path)
    pub display_name: String,     // defaults to key, overridable via init
}
```

`TaskDef` carries a `group` field that references the `GroupDef.key`:

```rust
pub struct TaskDef {
    pub name: &'static str,
    pub description: Option<&'static str>,
    pub group: &'static str,           // matches a GroupDef.key
    pub func: TaskFnKind,
    pub arg_metadata: ArgMetadataFn,
    pub ui_hint: Option<UiHint>,
}

pub enum TaskFnKind {
    Static(TaskFn),         // function pointer — from #[rnme::task], const-constructible
    Dynamic(DynamicTaskFn), // Arc<dyn Fn> — from InitContext::register_task(), captures state
}
```

The `#[task]` macro reads `__RNME_GROUP` (injected by the code generator) and sets `group` at compile time. It emits `TaskFnKind::Static(wrapper_fn)`.

Dynamic tasks are registered at init time via `InitContext::register_task()`, which leaks name/description/group strings to `&'static str` and wraps the closure in `TaskFnKind::Dynamic(Arc::new(closure))`. The runner drains dynamic tasks from each `InitContext` into the `Registry` after init hooks complete.

`InitContext.set_group_name()` modifies the `GroupDef.display_name` for that file's group. This separation means tasks don't need to know about display name overrides — the registry resolves group key to display name at lookup time.

## Generated Runner Crate

The runner crate is a binary crate in the generated workspace. It depends on all RUNME.rs lib crates via `[dependencies]` in its `Cargo.toml`. Cargo handles linking.

The linker only includes a crate's object files when actual symbols are referenced. `use x as _;` is a compiler-level construct that doesn't create linker-visible references — `inventory` registrations would be silently dropped. To prevent this, each generated lib crate exports a dummy function, and the runner calls it:

```rust
// Generated in each lib crate's lib.rs
pub fn __rnme_link() {}

// Generated in runner's main.rs
fn main() {
    root::__rnme_link();
    services_auth::__rnme_link();
    web_app::__rnme_link();

    // Build tokio runtime, then call into rnme::cli::run()
    // (which collects InitDefs, applies group overrides,
    //  builds the Registry, parses CLI args, dispatches to
    //  TUI/CLI/Agent through the engine).
}
```

The code generator already knows all crate names, so emitting these calls is trivial.

## Generated Crate Cargo.toml

Each RUNME.rs lib crate's `Cargo.toml`:

```toml
[package]
name = "services_auth"
version = "0.1.0"
edition = "2024"

[lib]
name = "services_auth"
path = "src/lib.rs"

[dependencies]
rnme = { path = "/absolute/path/to/rnme" }
monorepo-tools = { path = "/absolute/path/to/shared/tools" }
```

## Path Dependency Rewriting

RUNME.rs files can declare path-relative dependencies in their frontmatter:

```rust
//! [dependencies]
//! monorepo-tools = { path = "../shared/tools" }
```

The code generator resolves these at workspace generation time:

1. Parse the frontmatter dependency value
2. Detect `path = "..."` in the value
3. Resolve the relative path against the **original RUNME.rs file's directory** (not the cache directory)
4. Write the resolved absolute path into the generated `Cargo.toml`

The source code is unchanged — only the generated manifest is rewritten.

## Crate Naming

Each RUNME.rs file needs a unique, valid Rust crate name. Derived from the relative path of the RUNME.rs file from the discovery root (`src/bin/rnme/crate_name.rs`):

- `./RUNME.rs` → `root`
- `services/auth/RUNME.rs` → `services_auth`
- `web-app/RUNME.rs` → `web_app`

Rules: replace `/`, `-`, `.` with `_`. Prefix with `rnme_` if the name would be a Rust keyword or start with a digit. Collision detection if two paths produce the same name.

## Resolved Decisions

1. **Init ordering**: leaf-to-root. Siblings have no guaranteed order. Root runs last with the full picture.
2. **Group name override**: `TaskDef.group` is a key (relative path, set at compile time). Group display names live on `GroupDef`, overridable via `InitContext.set_group_name()`. Tasks don't carry display names directly.
3. **Single-file**: works fine — generates a one-file workspace (one lib crate + runner).

## Open Questions

1. **Root init API**: the root RUNME.rs may want to inspect/operate on child tasks and groups. Not needed now — `InitContext` is scoped to own-file config. Revisit when concrete use cases emerge.
