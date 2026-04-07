# TUI Implementation Plan

## Goal

Build the TUI described in `docs/tui_design.md`. The TUI is the primary interface for the power-user persona — task picker, log viewer, process sidebar, filtering, search, and process controls. It builds on top of the existing process management, log pipeline, and task system.

## Approach

The TUI is an exploratory implementation — we'll iterate as we see it working. The plan is organized as a series of vertical slices, each producing something visible and testable. Early phases focus on the rendering core and task execution plumbing; later phases add interactive features.

Work lives entirely in the `runme` library crate (`crates/runme/src/tui/`), with minor extensions to existing types (`TaskContext`, `ProcessHandle`) where the TUI needs hooks.

## Acceptance Criteria

- [ ] `runme` (no args) opens the TUI with task picker
- [ ] `runme <taskname>` starts a task and opens the log viewer
- [ ] Task function runs in-process; spawned processes appear in the sidebar
- [ ] Task tracing output (`info!`, `error!`, etc.) appears as "task" source in log viewer
- [ ] Log viewer supports preview/raw display modes and truncated/wrapped toggle
- [ ] Anchor-based scrolling works with variable-height entries
- [ ] Tail mode auto-follows; scrolling up enters pinned mode with `(+N new)` indicator
- [ ] Source filtering via sidebar toggles
- [ ] Live filter bar with `FilterExpr` DSL
- [ ] Search with `n`/`N` navigation and match highlighting
- [ ] Entry detail view showing all structured fields
- [ ] Process controls: stop, restart, signal from sidebar
- [ ] Clean shutdown on quit/signal/panic (children stopped, terminal restored)

## Status: in progress (Phase 5 complete)

## Context

### Design Documents

- `docs/tui_design.md` — full TUI design (source of truth)
- `docs/system_design.md` — system architecture, execution model, task lifecycle

### Existing Infrastructure

The TUI builds on these implemented primitives (see `docs/tui_design.md` § Existing Infrastructure for details):

- **`TaskContext`** — `exec()`, `spawn()`, output capture
- **`ProcessHandle`** — signal, stop, wait, output buffer access
- **`LogEntry`** / **`OutputBuffer`** / **`LogStore`** — log records, ring buffers, multi-source composition, broadcast subscriptions
- **`FilterExpr`** — Lucene-style filter DSL with parser and evaluator
- **`Search`** — text search with match byte ranges and context windows
- **`Registry`** — task discovery via `inventory`, lookup, execution
- **`Cmd`** — command builder with structured and shell modes

### Key Dependencies to Add

- `ratatui` — terminal UI framework
- `crossterm` — terminal backend (event handling, raw mode)
- `tracing` + `tracing-subscriber` — task function logging
- `nucleo` or `fuzzy-matcher` — fuzzy search for task picker

---

## Phase 1: App Shell & Task Execution

**Goal:** A runnable TUI that can launch a task, capture its output, and display raw text in a terminal. The minimum viable "it works" moment.

This phase has two independent streams that converge at the end.

### Task 1a: Tracing integration

Extend the `runme` library so that task functions can emit structured logs via `tracing` macros.

**Work:**
- Add `tracing` and `tracing-subscriber` dependencies to `crates/runme/Cargo.toml`
- Create `crates/runme/src/tracing.rs` — a custom `tracing::Layer` that converts `tracing::Event`s into `LogEntry`s and pushes them to an `OutputBuffer`
  - Map tracing level → `LogEntry.level`
  - Map tracing message → `LogEntry.message`
  - Map tracing fields → `LogEntry.fields`
  - Source name: `"task"`
- Re-export `tracing` macros (`info!`, `error!`, `warn!`, `debug!`, `trace!`) from `runme::prelude`
- Add `ctx.stop_all()` to `TaskContext` — stops all processes spawned through this context

**Verification:** Unit test that installs the layer, emits `info!(key = "val", "hello")`, and verifies a `LogEntry` appears in the buffer with the correct fields.

### Task 1b: App shell & event loop

Set up the ratatui application skeleton.

**Work:**
- Add `ratatui` and `crossterm` dependencies to `crates/runme/Cargo.toml`
- Create `crates/runme/src/tui/mod.rs` — module root
- Create `crates/runme/src/tui/app.rs` — `App` struct holding `AppState`, `run()` method
- Create `crates/runme/src/tui/event.rs` — async event loop:
  - `tokio::select!` over terminal events, log entries (broadcast receiver), process status changes, render ticks
  - Dirty flag rendering at 60fps cap
- Terminal setup/teardown: enter raw mode, enable alternate screen, restore on exit
- Panic hook: `std::panic::set_hook` to restore terminal before unwinding
- Signal handling: catch SIGINT/SIGTERM, trigger clean shutdown (stop children, restore terminal)
- `q` key quits
- Display a status bar at the bottom showing "runme" and a placeholder

**Verification:** `cargo run -p runme -- --tui` (or equivalent) shows a blank screen with a status bar; `q` exits cleanly; Ctrl-C exits cleanly; a forced panic restores the terminal.

### Task 1c: Wire it together

Connect task execution to the TUI.

**Work:**
- Create `crates/runme/src/tui/runner.rs` — task execution orchestrator:
  - Accept a `TaskDef` and a `LogStore`
  - Install the tracing layer (from 1a)
  - Create a `TaskContext` for the task
  - `tokio::spawn` the task function
  - Track `TaskStatus` transitions (Setup → Ready/Done/Failed)
  - When the task spawns processes via `ctx`, register their `OutputBuffer`s with the `LogStore`
- Modify `TaskContext` to emit an event when `spawn()` is called (callback or channel) so the TUI can learn about new processes
- The app shell (from 1b) subscribes to the `LogStore` broadcast and renders raw log text in the main area — no formatting, no sidebar, just dumping lines
- Implement basic `ScrollState::Tail` — new entries push the view

**Verification:** Launch a task that calls `info!("hello")` and `ctx.spawn("echo world")`. Both lines appear in the TUI.

---

## Phase 2: Log Viewer Core

**Goal:** The log viewer renders formatted entries with anchor-based scrolling. This is the rendering engine that everything else builds on.

### Task 2a: Entry rendering

**Work:**
- Create `crates/runme/src/tui/render.rs` — entry rendering logic:
  - Given a `LogEntry`, terminal width, `DisplayMode`, and wrap setting, produce styled `ratatui::text::Line`s
  - Preview mode: fixed-width columns for timestamp, level (color-coded), source (color-coded), message
  - Raw mode: original text
  - Truncated: exactly 1 `Line` per entry, clipped at terminal width
  - Wrapped: multiple `Line`s per entry, wrapping at terminal width
  - Return the visual height (number of `Line`s) alongside the rendered lines
- Source color assignment: maintain a palette, assign colors by source name, consistent within session

**Verification:** Unit tests rendering entries in each mode/wrap combination, verifying line counts and column alignment.

### Task 2b: Anchor-based viewport

**Work:**
- Create `crates/runme/src/tui/viewport.rs` — the core scrolling engine:
  - Input: anchor (entry index + Y position), visible entry list, viewport height, render function
  - Output: which entries to draw at which Y positions
  - Algorithm: render the anchor entry at its Y position, then fill upward and downward
  - Handle partial entries (anchor entry's first line might be above the viewport top)
- Integrate into the app: replace the raw text dump from Phase 1 with viewport-driven rendering
- Implement `ScrollState::Pinned` — j/k move the anchor entry, re-render
- Implement `ScrollState::Tail` — anchor is last entry, pinned to bottom
- Tail → pinned transition: any upward scroll switches mode
- Pinned → tail: `G` jumps to last entry
- Status bar shows `TAIL` or `entry N / total (+M new)`

**Verification:** Launch a task that produces continuous output. Verify tail mode follows. Press `k` to pin. Verify `(+N new)` counter increments. Press `G` to return to tail.

### Task 2c: Display mode toggles

**Work:**
- Key binding to toggle preview/raw mode
- Key binding to toggle truncated/wrapped
- On toggle: re-render from same anchor entry (heights reflow, anchor stays put)
- On terminal resize: same behavior — re-render from anchor

**Verification:** Toggle modes while viewing output. Anchor entry stays visible. Wrapped mode shows full content of long lines.

---

## Phase 3: Process Sidebar

**Goal:** The sidebar shows the task and its spawned processes with status indicators.

**Work:**
- Create `crates/runme/src/tui/sidebar.rs` — sidebar widget:
  - Three sections: Task (top), Running processes, Completed processes
  - Task status: SETUP / READY / DONE / FAIL
  - Process status: RUN / DONE / FAIL / STOP with color coding
  - Highlight current selection
- Layout: sidebar on the left (fixed width initially), log viewer on the right, status bar at the bottom
  - Use `ratatui::layout::Layout` with horizontal split
- Tab key toggles focus between sidebar and log viewer
- When sidebar is focused: j/k moves selection, Enter/Space toggles source visibility in the log view
- Source visibility: toggling a process in the sidebar updates `LogViewState.source_filter`, which rebuilds the visible entry list
- Number keys 1-9 toggle source visibility from log viewer (mapped to sidebar order)
- `a` shows all sources

**Verification:** Launch a task that spawns multiple processes. Sidebar shows them with correct statuses. Toggle a source off — its entries disappear from the log. Toggle back — they reappear. Anchor stays stable through filter changes.

---

## Phase 4: Filter Bar

**Goal:** Live filtering using the existing `FilterExpr` engine.

**Work:**
- Create `crates/runme/src/tui/filter.rs` — filter input widget:
  - Text input at the bottom of the screen (status bar area)
  - `f` enters filter input mode; `Esc` cancels; `Enter` confirms
  - As the user types, parse the expression and rebuild the visible entry list
  - Parse errors shown inline (red text) without clearing the view — last valid filter stays active
  - `Ctrl-u` clears the input
- Filter pipeline: source filter → expression filter → visible entry list
- Placeholder text when filter is empty: `filter: level:error AND source:api ...`
- The visible entry list rebuild is the main cost — for typical buffer sizes (<500K entries) this should be sub-16ms. If not, add debouncing.

**Verification:** Type `level:error` — only errors visible. Clear filter — all entries return. Type an invalid expression — red error, previous results stay. Anchor survives filter changes.

---

## Phase 5: Search

**Goal:** `/` search with navigation and highlighting.

**Work:**
- Create `crates/runme/src/tui/search.rs` — search input and match tracking:
  - `/` enters search mode; typing updates pattern; `Enter` confirms; `Esc` cancels
  - On confirm: scan visible entries for matches, store matching entry indices
  - `n` jumps to next match (sets anchor to that entry), `N` jumps to previous
  - Status bar shows `[match 3/17]`
- Search highlighting in the renderer:
  - When rendering an on-screen entry, check if it has search matches
  - Compute match byte ranges (using existing `Search` infrastructure) and overlay highlight style
  - Current match gets a distinct highlight color vs other matches
- Incremental: when new entries arrive in tail mode, check against active search pattern

**Verification:** Search for a term. Matches highlighted. `n`/`N` navigates between them. Status bar shows position.

---

## Phase 6: Entry Detail & Process Controls

**Goal:** Expand a log entry to see all fields. Stop/restart/signal processes from the sidebar.

### Task 6a: Entry detail view

**Work:**
- `Enter` on a log entry opens a detail pane (bottom half or overlay)
- Shows all structured fields: timestamp, level, source, message, then all `LogEntry.fields` with flattened key paths
- Raw text at the bottom
- j/k scrolls within the detail pane (this is the one place we navigate within an entry)
- `Esc`/`q` closes detail, returns to log viewer
- `n`/`N` closes detail and jumps to next/previous entry (or next/previous search match if searching)
- `y` copies entry to clipboard (OSC 52)

**Verification:** Expand a JSON log entry. All fields visible. Scroll within detail. Close and return.

### Task 6b: Process controls

**Work:**
- Sidebar focused, process selected:
  - `s` — stop: calls `ProcessHandle::stop()`
  - `r` — restart: stop, then re-launch the same `Cmd`
  - `S` — send SIGHUP
- Restart preserves log buffer, appends new output with a separator entry
- TaskContext needs to store the `Cmd` used for each spawn so restart can replay it

**Verification:** Spawn a long-running process. Stop it from sidebar — status changes to STOP. Restart it — new output appears after separator.

---

## Phase 7: Task Picker

**Goal:** Fuzzy-find task selection on startup.

**Work:**
- Create `crates/runme/src/tui/picker.rs` — task picker view:
  - Full-screen overlay on startup (or when returning from a completed task)
  - List all tasks from `Registry`, grouped by `TaskDef.group`
  - Group display names from `GroupDef` (root group shown as `.`)
  - Browse mode: j/k navigation, groups collapsible with Enter
  - Fuzzy search: start typing to filter; hierarchy flattens to ranked list
  - Enter launches selected task, transitions to log viewer
- Add `nucleo` or `fuzzy-matcher` dependency
- `AppMode::TaskPicker` as the initial mode when no task name is provided

**Verification:** Launch `runme` with no args. Picker shows all tasks grouped. Type to filter. Select and launch — transitions to log viewer with task running.

---

## Phase 8: Polish

**Goal:** Loose ends and quality-of-life improvements.

**Work (pick as needed):**
- Export: `:export` command or keybinding to dump visible log to file (raw text, JSON lines)
- Clipboard copy: `y` in normal mode copies selected entry
- Crash surfacing: transient notification when a process fails while filtered out or scrolled away
- Mouse support: scroll wheel, click to select entry, click sidebar
- Filter history: `Up`/`Down` in filter input cycles through previous filters
- Help overlay: `?` shows keyboard shortcuts
- Sidebar collapse: toggle key to give log viewer full width

---

## Dependencies Between Phases

```
Phase 1a (tracing) ──┐
                      ├──> Phase 1c (wire together) ──> Phase 2 ──> Phase 3 ──> Phase 4 ──> Phase 5 ──> Phase 6
Phase 1b (app shell) ─┘                                                                                    │
                                                                                                            v
                                                                                                        Phase 7
                                                                                                            │
                                                                                                            v
                                                                                                        Phase 8
```

Phases 1a and 1b can be done in parallel. Everything else is sequential — each phase builds on the previous. Phase 7 (task picker) depends on the log viewer being functional but not necessarily on search/detail/controls. Phase 8 is a grab bag that can happen in any order.

## Human Review Gates

1. **After Phase 1c** — Verify the execution plumbing works: task runs, tracing output appears, spawned processes are visible. This is the foundation.
2. **After Phase 2** — Verify the rendering core: anchor-based scrolling, display modes, tail/pinned behavior. This is the hardest piece to get right.
3. **After Phase 5** — The core interactive experience is complete (viewer + sidebar + filter + search). Good checkpoint before adding detail view and controls.

## Key Files

**New (TUI):**
- `crates/runme/src/tui/mod.rs` — module root
- `crates/runme/src/tui/app.rs` — App struct, state, run loop
- `crates/runme/src/tui/event.rs` — async event handling
- `crates/runme/src/tui/render.rs` — entry rendering (preview/raw, truncated/wrapped)
- `crates/runme/src/tui/viewport.rs` — anchor-based scrolling engine
- `crates/runme/src/tui/sidebar.rs` — process sidebar widget
- `crates/runme/src/tui/filter.rs` — filter bar input and pipeline
- `crates/runme/src/tui/search.rs` — search input, match tracking, highlighting
- `crates/runme/src/tui/picker.rs` — task picker with fuzzy search
- `crates/runme/src/tui/runner.rs` — task execution orchestrator

**New (lib support):**
- `crates/runme/src/tracing.rs` — tracing::Layer → LogEntry adapter

**Modified:**
- `crates/runme/src/task.rs` — `TaskContext`: `stop_all()`, spawn notification callback, store Cmd per spawn
- `crates/runme/src/lib.rs` — add `pub mod tui;`, `pub mod tracing;`
- `crates/runme/src/prelude.rs` — re-export tracing macros
- `crates/runme/Cargo.toml` — add ratatui, crossterm, tracing, tracing-subscriber, nucleo/fuzzy-matcher
