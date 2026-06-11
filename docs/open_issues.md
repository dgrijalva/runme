# Open Issues

## TUI keybinding layout needs a rework

**18:43** — Current shortcut scheme isn't satisfying. Want to study LazyGit (and a bit of Vim) for inspiration on key layouts appropriate for a system like this.

- _Effort:_ Moderate — keybinding plumbing already centralized in `src/tui/keys.rs`, but a real redesign means rethinking modes, discoverability, and possibly conventions across the whole TUI
- _Assessment:_ Current scheme (in `src/tui/keys.rs`) is a flat per-mode `match` on `KeyCode`. It mostly mirrors Vim motions (`j`/`k`/`g`/`G`/`Ctrl-d`/`Ctrl-u`/`/`/`n`/`N`) plus ad-hoc single-letter actions (`s`/`S` signals, `a` show-all, `f` filter, `c` copy menu, `e` export, `y` yank, `w` wrap, `d` fields, `v`/`m` raw, `\` sidebar, `1-9` source toggles, Enter detail). No leader key, no which-key/help overlay, no consistent pane-numbering nav, and `1-9` overloads source toggles in a way that won't scale beyond 9 sources. Worth pulling apart what LazyGit does well: numbered pane focus, context-sensitive footer hints, multi-stage menus (e.g. `c` → commit submenu), and a global `?` cheatsheet
- _Concern:_ Affects muscle memory — once a scheme is published, churn is annoying. Pre-release status (per CLAUDE.md) means now is the right window. Also: a redesign should probably be paired with a discoverability mechanism (footer hints / `?` overlay), otherwise users won't find the new bindings
- _Inspiration sources to look at:_ LazyGit (panes + which-key menus + footer hints), LazyDocker (similar TUI shape, log-heavy), k9s (resource navigation), Vim/Helix (motion + text-object grammar). Helix may be more relevant than Vim proper since its keybinding philosophy is closer to a curated app than a programmable editor

## Surface task summaries in the TUI

`ctx.summary` lands in the MCP report path but the TUI doesn't read it — decide if/where to show it.

## Speed up runner build time

**15:16** — Build time of the generated runner crate is the dominant contributor to launch time. Already building in debug. Application perf doesn't matter — `rnme` is a supervisor. Look at what Bevy recommends for fast iteration builds and pull what applies.

- _Effort:_ Small per-knob, but the search space is wide — likely a series of experiments rather than one fix. Some options need a Linux-only path (mold/lld) so cross-platform care matters
- _Assessment:_ Generated workspace in `src/bin/rnme/` currently does a vanilla `cargo build` with no custom `[profile.dev]` overrides (no `opt-level`, `codegen-units`, `debug`, `lto`, `incremental` set in the generated `Cargo.toml`). Lots of headroom. Concrete levers worth trying, roughly in order of expected ROI:
    - **`debug = 0`** (or `"line-tables-only"`) in the generated dev profile — debuginfo is a huge fraction of debug-build wall time and we don't run a debugger on the runner
    - **`codegen-units = 256`** for the runner crate — already the default in dev (256), but verify; pin it explicitly so a user-level config can't override
    - **Cranelift backend** (`-Zcodegen-backend=cranelift` on nightly, or stable via `cargo-codegen-backend`) — Bevy's biggest single-knob win for dev builds, often 30-50% faster codegen
    - **`rnme` as `dylib`** — Bevy's `dynamic_linking` trick. The runner depends on `rnme` (the library); link it dynamically so each rebuild only relinks the runner, not the whole `rnme` crate graph
    - **`share-generics = true`** (`-Zshare-generics=y`) — less monomorphization duplication across the workspace's lib crates
    - **Faster linker** — `lld` on macOS (already default-ish on recent Xcode), `mold` on Linux. `rustflags = ["-C", "link-arg=-fuse-ld=lld"]` in the generated `.cargo/config.toml`
    - **`incremental = true`** — already default in dev, but double-check we're not accidentally disabling it via env (e.g. `CARGO_INCREMENTAL=0` from somewhere)
    - **Strip the runner crate down** — anything we can move from per-RUNME-lib-crate compile work into `rnme` core (compiled once, cached forever) is pure win
    - **Persistent target cache** — already cache-dir-keyed; verify nothing invalidates it on every run (e.g. timestamp-bumping a generated source file with identical content)
    - **Pre-warmed `rnme` crate** — ship a precompiled `rnme` rlib that the runner workspace just links against, so first-time-after-version-bump pain is bounded
- _Concern:_ Some knobs are nightly-only (`-Z share-generics`, cranelift via `-Z`). Acceptable for `rnme` since we already require nightly for edition 2024. Linker choice needs OS-specific config in the generated `.cargo/config.toml`
- _Inspiration:_ Bevy's "fast compiles" config (https://bevy.org/learn/quick-start/getting-started/setup/), the Rust compile-time perf book

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:

- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line

## Log grouping / context views for debugging

**Feature idea:** Ways to group or contextualize log entries when debugging. These may be separate features that share a theme rather than one mechanism.

- **Filter with context (`grep -C`-style):** Show each match plus N entries above and below, with a blank line separating match groups. Useful when the surrounding lines are what actually explain a match
- **Group by data (e.g., request ID):** When a server is juggling concurrent requests, interleaved output is unreadable. Let the user pick a key (request id, trace id, session id, ...) and reassemble entries into per-key streams. Probably needs the field extractor to surface arbitrary structured fields, not just timestamp/level/message
- **Arbitrary task-defined grouping:** Expose a hook so a task can define its own grouping/labeling logic over its output. Anywhere user code gets a concrete interface to influence tool behavior is a win — same philosophy as `ctx.summarize`. Could be a function that takes a `LogEntry` and returns a group key (or `None`)
- _Effort:_ Variable. `grep -C` context is small (filter/render layer in `src/log/filter.rs` + `src/tui/render.rs`). Group-by-key is moderate — needs richer field extraction and a new view mode. Task-authored grouping is moderate-to-large — new `TaskContext` API + plumbing through to the log store
- _Assessment:_ Worth treating as a family of features unified by "give the user lenses on log data." The field extraction pipeline (`src/log/extract.rs`) and store (`src/log/store.rs`) probably need extension to carry structured field maps per entry, which all three variants would build on
- _Open questions:_ Is grouping a view-time concern (TUI re-renders the same store with a different lens) or a store-time concern (entries are tagged on ingest)? View-time is more flexible; store-time is cheaper at render. Does task-authored grouping run synchronously in the parsing pipeline, or async over the store?

## `SpawnBuilder::.await` returns post-spawn, not post-ready

**13:42** — `SpawnBuilder::.await` at `src/execution/engine.rs:914` returns post-spawn, not post-ready. Naive code like `spawn().ready_on_port(...).ready_timeout(...).await` looks like a complete readiness protocol but actually races — the await returns before the probe runs. Worth considering a `.ready().await` builder terminator that spawns + waits for ready + propagates the timeout error.

- _Effort:_ Small — new method on `SpawnBuilder` that composes `.await` + `handle.wait_ready()`
- _Context:_ Surfaced alongside the `ready_timeout` bug (now fixed). The timeout fix means readiness failures propagate correctly when callers do call `wait_ready`, but the footgun of forgetting to await it remains

## Built-in cargo task helpers

**Feature idea:** Library-side support for the common cargo workflows (check / build / test, maybe clippy / fmt / doc). Users register them during `#[rnme::init]` with a single fn call and get smart summaries + optional watch behavior for free.

- _Example:_ `cargo_tasks::register(ctx).check().build().test().watch();` (shape tbd) — produces ready-to-run tasks with sensible names, descriptions, and post-processed summaries (errors/warnings counts, failing tests, slow tests, etc.)
- _Effort:_ Moderate — a new module/feature in `rnme` core that builds on `Cmd` + the dynamic task registration path (`InitContext::register_task()`). Cargo output parsing for summaries is the biggest chunk; `--message-format=json` makes it tractable
- _Assessment:_ Natural pairing with the planned `ctx.summarize` API — these tasks would be the canonical first consumers. Cargo is universal enough across Rust projects that built-in support pays off vs. copy-pasting boilerplate. Watch mode could lean on `cargo-watch` initially, or implement file-watching natively via `notify`. Dynamic registration already supports this shape (closures + leaked `&'static str` names), so no new plumbing required
- _Concern:_ Scope creep — once we ship "built-in cargo," people will want npm, pnpm, go, make, etc. Worth deciding up front whether this is "blessed first-party helpers" or "first example of a pluggable helper ecosystem"
- _Open questions:_ Where does it live — `rnme::cargo` module, separate `rnme-cargo` crate, or feature-gated? What's the registration API shape (builder, free functions, struct config)? Are watch tasks separate task entries or a flag on the base task? Does this also surface `cargo run`-style long-lived processes, or stay focused on the check/build/test triad?
