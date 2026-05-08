# Open Issues

## TUI keybinding layout needs a rework

**18:43** — Current shortcut scheme isn't satisfying. Want to study LazyGit (and a bit of Vim) for inspiration on key layouts appropriate for a system like this.

- _Effort:_ Moderate — keybinding plumbing already centralized in `src/tui/keys.rs`, but a real redesign means rethinking modes, discoverability, and possibly conventions across the whole TUI
- _Assessment:_ Current scheme (in `src/tui/keys.rs`) is a flat per-mode `match` on `KeyCode`. It mostly mirrors Vim motions (`j`/`k`/`g`/`G`/`Ctrl-d`/`Ctrl-u`/`/`/`n`/`N`) plus ad-hoc single-letter actions (`s`/`S` signals, `a` show-all, `f` filter, `c` copy menu, `e` export, `y` yank, `w` wrap, `d` fields, `v`/`m` raw, `\` sidebar, `1-9` source toggles, Enter detail). No leader key, no which-key/help overlay, no consistent pane-numbering nav, and `1-9` overloads source toggles in a way that won't scale beyond 9 sources. Worth pulling apart what LazyGit does well: numbered pane focus, context-sensitive footer hints, multi-stage menus (e.g. `c` → commit submenu), and a global `?` cheatsheet
- _Concern:_ Affects muscle memory — once a scheme is published, churn is annoying. Pre-release status (per CLAUDE.md) means now is the right window. Also: a redesign should probably be paired with a discoverability mechanism (footer hints / `?` overlay), otherwise users won't find the new bindings
- _Inspiration sources to look at:_ LazyGit (panes + which-key menus + footer hints), LazyDocker (similar TUI shape, log-heavy), k9s (resource navigation), Vim/Helix (motion + text-object grammar). Helix may be more relevant than Vim proper since its keybinding philosophy is closer to a curated app than a programmable editor

## Task-authored output summaries (`ctx.summarize`)

**Feature idea:** Let a task post-process its own output and publish a summary string via something like `ctx.summarize(s)`. When agent/MCP mode lands, an agent that runs a task could request the summary instead of pulling raw logs — far cheaper context-wise.

- _Example:_ A `cargo build` task post-processes the build output (errors, warnings counts, failing crate) into a useful summary to hand back to Claude
- _Effort:_ Moderate — needs a per-session summary slot on `TaskSession`/status, an API surface on `TaskContext`, and (eventually) an MCP tool that prefers summary over logs when present
- _Assessment:_ Fits naturally next to existing per-session state (`TaskStatus`, `processes`). The task is the right place to author this since only the task knows what's interesting in its own output. Summary is just a `String` (or maybe `Option<String>` + timestamp); no need for streaming/structure in v1
- _Open questions:_ Single summary or append-only stream? Overwrite semantics on re-run? Should summaries persist with completed tasks (probably yes, since logs do)? Does the TUI surface them anywhere, or are they purely for programmatic consumers?

## Filter occasionally collapses to a single result

**14:17** — Applying a filter expression in the TUI sometimes shows only one matching line even when multiple entries clearly match. Repro conditions unknown.

- _Effort:_ Needs dedicated exploration — repro is the hard part
- _Assessment:_ Filter pipeline is `App::visible_line_indices()` / `visible_log_lines()` (src/tui/app.rs:301, src/tui/app.rs:323), which composes focus filter + manual hides + `log_filter::matches(expr, entry)`. Worth checking: (a) is `effective_visible_sources()` returning a stale/over-narrow set when sources arrive after the filter is applied? (b) does the viewport's visible-index list get out of sync with `log_lines` after a re-filter, so it renders one line rather than scrolling? (c) is search vs filter being conflated — `n`/`N` jumps to next match and could leave the viewport on a single-line slice
- _Concern:_ Hard to fix without a repro. Next time it happens, capture: filter expr, source set in sidebar, whether focus filter is active, whether search was used recently

## Soft vs hard restart

**14:17** — Two restart modes. `r` = soft restart: a cooperative signal the task can opt into. `R` = hard restart: current behavior (kill + respawn). Mechanism: `TaskContext` exposes a restart channel/signal that the task can `take()`. If taken, soft restart sends through that channel and the task decides what to do (e.g., reload config, re-exec a child, drain). If not taken, soft and hard behave identically. CLI mode could map `SIGHUP` to soft restart on the same principle.

- _Effort:_ Moderate — new `TaskContext` API surface, restart routing in the runner, TUI key wiring, and signal handling in CLI mode
- _Assessment:_ Fits the existing `TaskContext` shape (it already owns process lifecycle plumbing). Restart channel is probably a `oneshot`-per-restart or a `tokio::sync::Notify` the task can `take()` once. "Take" semantics matter: only one consumer, and the runner needs to know whether it was taken so it can decide soft-vs-hard fallback. CLI mode parallel is clean since `SIGHUP` is the conventional cooperative-reload signal anyway
- _Open questions:_ What does "the task" mean for soft restart — the top-level task fn, or any spawned child it owns? Is the signal one-shot per restart, or a stream the task subscribes to for the lifetime of the run? Does soft restart wait for the task to finish handling, or fire-and-forget? What happens if the task takes the signal but never responds — timeout into hard restart?

## Bare `spawn!` macro with auto-traced context

**14:17** — Add a `spawn!` macro to the prelude that wraps `tokio::spawn` (or equivalent) and automatically threads tracing context through, so logs emitted from the spawned future are correctly attributed to the originating task/source.

- _Effort:_ Small-to-moderate — proc macro or `macro_rules!` in `rnme-macros` or the prelude, plus integration with the existing `tracing_layer.rs`
- _Assessment:_ Existing `src/tracing_layer.rs` already does source attribution for the log engine. The macro just needs to capture the current `tracing::Span` (or whatever context the layer keys on) and `.instrument()` the spawned future — standard tracing-tokio pattern. Likely a thin `macro_rules!` is enough; no proc macro needed unless we want to also rewrite the body. Prelude export keeps it ergonomic from RUNME.rs files
- _Open questions:_ Does this wrap `tokio::spawn` only, or also `ctx.spawn()` (process spawn)? They're different beasts — process spawn already has source attribution via the runner; the gap is in-process `tokio::spawn` for ad-hoc async work inside a task. Probably this macro is specifically for the latter

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:

- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line
