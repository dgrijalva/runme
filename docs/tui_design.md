# TUI Design

## Overview

The TUI is the primary interface for the power-user persona. It runs multiple tasks concurrently, observes them through the engine's graph snapshot and log store, and gives the user keyboard-driven control over what's running and what's visible. The log viewer is the centerpiece — most time is spent reading logs, so that experience must be excellent.

The TUI is a thin frontend on top of the engine (`src/execution/`). It holds an `EngineHandle`, watches the graph for changes, subscribes to the log store, and emits control messages (spawn, kill, quit) when the user acts. It does not own task lifecycle bookkeeping; that all lives in the engine. The old `TaskRunner` type that used to wrap multi-task state has dissolved into a stub of re-exports (`src/tui/runner.rs`) — what's left of "TUI plumbing" is rendering and input handling.

For the engine model the TUI consumes — synthetic root, recursive graph, control protocol, cancellation — see `runtime_engine_design.md`.

## Foundations

The TUI builds on these primitives, all engine-owned or shared:

- **`EngineHandle`** — `graph: watch::Receiver<GraphSnapshot>` for change-detection, `log_store: Arc<Mutex<LogStore>>` for log subscriptions, `subscribe_logs()` for broadcast, `source_ids_for(task_id)` for filter computation, `spawn_task` / `kill_task` / `kill_all` / `kill_process` / `quit` for control.
- **`GraphSnapshot`** — immutable map of `TaskId -> TaskNode { name, parent, children, status, processes }`. The TUI rebuilds its sidebar and source labels from this each frame.
- **`LogEntry`** — universal log record: `raw`, `parsed`, `source: TaskId`, `seq`, extracted fields (`timestamp`, `level`, `message`), arbitrary `fields: HashMap`.
- **`LogStore`** — multi-source aggregator with broadcast, source filtering, grouping. Engine-owned, frontend-shared.
- **`FilterExpr`** — Lucene/Datadog-style filter DSL (`level:error AND source:auth`, negation, regex, wildcards, comparisons).
- **`Search`** — full-text search with match highlighting, regex, case sensitivity, context windows.

## Execution Model

The engine runs multiple tasks under a synthetic root. The TUI displays them all. There is no "current task" in the engine — `AppState::current_task` exists only so the `r` (restart) key knows what to relaunch and so single-task command invocations (`rnme build`) have something to wait on at startup.

Multiple tasks coexist freely. The picker overlay can be opened at any time to spawn another. Tasks may be cancelled individually (`k k`) or as a group (`k a`). When tasks finish, they stay in the sidebar with their logs accessible. The TUI shell stays open until explicit quit (`q`).

A single global `tracing` subscriber is installed once, in the engine. It routes events into the right per-task buffer using a task-local `TASK_TRACING_CTX`. Task code's `info!`/`error!`/etc. interleave with subprocess output in the log viewer, distinguished by source label and color.

### Startup

- `rnme` (no arguments): TUI opens with the picker overlay over an empty shell. Same as `t` from any state.
- `rnme <task>`: TUI opens with the task already spawning. Sidebar shows it; logs flow in; picker is closed.

### Shutdown

`q` opens a confirmation modal if any task other than root is still in `Setup`/`Ready`; otherwise it quits immediately. Confirming runs `engine_handle.quit().await` — cancel the whole subtree under root, root body returns, `engine.shutdown()` joins. SIGINT (Ctrl-C) takes the same path. Terminal restoration is wired to `std::panic::set_hook` so a panic doesn't leave the user with a broken terminal.

## Layout

### Primary View

```
+--[ tasks ]-------------+--[ logs ]----------------------------------------------+
|   All tasks            | 12:01:03.410  INFO  [1] task   Starting deployment...   |
|                        | 12:01:03.412  INFO  [2] api     Request handled GET     |
| > deploy      [READY]  | 12:01:03.415  DEBUG [3] worker  Processing job 4821     |
|     api-server [RUN]   | 12:01:03.418  ERROR [2] api     Connection refused      |
|     worker    [RUN]    | 12:01:03.420  INFO  [2] api     Retrying connection     |
|     migrate   [DONE]   | 12:01:03.421  WARN  [3] worker  Queue depth > 100       |
|                        | 12:01:03.425  INFO  [2] api     Connected to db:5432    |
|   build       [DONE]   |                                                         |
|     cargo build [DONE] |                                                         |
|                        |                                                         |
+------------------------+---------------------------------------------------------+
| [filter: level:error ]                                               TAIL | 842 |
+----------------------------------------------------------------------------+-----+
```

**Sidebar** (left): built each frame from `GraphSnapshot` via `build_sidebar_entries_from_graph`. Top entry is always **All tasks** — selecting it focuses the synthetic root and shows the unfiltered merged log. Below it, direct children of root render in spawn order with their child processes (and any descendant tasks) nested under them. Completed tasks remain visible.

Status tags (`SETUP`, `READY`, `DONE`, `FAIL`, `RUN`, `STOP`, `CANCELLED`, `TIMEOUT`) come from the snapshot. Color-coded — see `theme.rs`. Sidebar focus drives log filtering: navigating to a task focuses it, computing the visible source set as `engine.source_ids_for(task_id)`.

Source identity is unified: every task and every process has a `TaskId` from the same allocator. `LogEntry.source` is a `TaskId`. The source column in the log viewer renders as `[N] label` where `N` is the entry's position in the visible source list and `label` is the task name or process command. Duplicate labels (e.g., two `cargo build`) are disambiguated first by source color, then by the `[N]` prefix.

**Log viewer** (main area): occupies the remaining width. Each line shows timestamp, level, source tag, message. Structured entries show the extracted `message`; raw text entries show the raw line. Lines wrap (`w` toggle) or truncate.

**Status bar** (bottom): active filter (editable in-place), scroll mode (`TAIL` or `LINE 4821 (+47 new)`), entry counts, search status.

### Detail View: Expanded Log Entry

`Enter` on a log entry opens an overlay with all extracted fields, flattened key paths, and the raw text at the bottom. `Esc` or `q` closes. Scrollable with `j`/`k`.

### Picker Overlay

`t` opens the picker over the existing TUI shell. The picker is **always an overlay**, never a full-screen mode — sidebar and logs stay rendered behind it. Re-entrant from any state. On startup with no task, the shell is empty (just "All tasks" in the sidebar) and the picker opens automatically; visually identical to opening it later.

```
+--[ pick a task ]---------------------------------------------------+
| > web                                                              |
|                                                                    |
| .                                                                  |
|   start              Start all services                            |
|   clean              Clean all build artifacts                     |
|                                                                    |
| services/auth                                                      |
|   build              Build the auth service                        |
|   test               Run auth service tests                        |
|                                                                    |
| web-app                                                            |
|   dev                Start webpack dev server                      |
|   build              Production build                              |
+--------------------------------------------------------------------+
```

Tasks are grouped by `TaskDef.group`. Display name comes from `GroupDef.display_name` (defaults to relative path; overridable via `#[rnme::init]::set_group_name`). Root group renders as `.`.

- **Browse**: the full hierarchy. Navigate with `j`/`k`.
- **Fuzzy search**: typing filters across task names, descriptions, group names. Matches the fully qualified name, so `auth build` finds `services/auth > build`.
- **Enter**: launches the selected task by emitting `EngineHandle::spawn_task(def, args)`. The new child appears in the sidebar; the overlay closes.
- **Esc**: closes the overlay without launching. **Ctrl-C** quits the TUI entirely. **`q` is just text** in the picker, not a quit.

The picker is designed to grow a split layout for an argument-input form when launching tasks that take args — that's not built yet but the overlay shape accommodates it.

## App State Model

```
AppState
  mode: AppMode                              // active modal-style submode
  picker_open: bool                          // picker overlay visible
  quit_confirm: bool                         // quit-confirmation modal visible
  engine: Option<EngineHandle>               // engine for control + observation
  log_store: Arc<Mutex<LogStore>>            // cloned from engine
  log_lines: Vec<LogEntry>                   // tail of merged log store
  sidebar: SidebarState                      // focus + selection
  sidebar_entries: Vec<SidebarEntry>         // rebuilt each frame from snapshot
  focus_filter: HashSet<TaskId>              // sources visible from focused entry
  hidden_sources: HashSet<TaskId>            // user-hidden sources (composes with focus)
  scroll: ScrollState                        // tail or pinned anchor
  display_mode: DisplayMode                  // Preview or Raw
  wrap: bool                                 // truncated or wrapped
  filter_input: FilterInputState
  search: SearchState
  source_colors: SourceColors
  current_task_id: Option<TaskId>            // most recently launched (for r/restart)
  current_task: Option<&'static TaskDef>     // for restart
  ...
```

`AppMode` is for modal-style submodes that take over the input layer:

```
enum AppMode {
    Normal,
    FilterInput,
    SearchInput,
    Help,
    EntryDetail,
    ProcessDetail,
    CopyMenu,
    KillMenu,
}
```

The picker and quit-confirm are **not** modes — they're flags (`picker_open`, `quit_confirm`) that overlay the existing shell, since the shell stays rendered behind them. The picker can open from any mode.

### Source visibility composition

Visible sources = (`focus_filter` if non-empty, else "all sources") **minus** `hidden_sources`. Sidebar focus rewrites `focus_filter` via `engine.source_ids_for(focused_task)`. Manual toggles (`Space`/`Enter` on a sidebar process, number keys `1-9`) flip entries in `hidden_sources`. The two compose, so manual hides persist across focus changes (entries that aren't part of the current `focus_filter` simply stay hidden silently).

## Navigation & Interaction

### Keyboard Map

**Top level (any pane focus)**

| Key | Action |
|-----|--------|
| `?` | Help overlay |
| `q` | Quit (with confirmation if any task is running) |
| `Ctrl-c` | Quit immediately, no prompt |
| `t` | Open picker overlay (re-entrant) |
| `k` | Open kill submenu |
| `r` | Restart current task |
| `Tab` | Toggle sidebar focus |

**Normal mode (log viewer focused)**

| Key | Action |
|-----|--------|
| `Down` / `Up` | Move cursor by one entry |
| `Ctrl-d` / `]` / `PgDown` | Half page down |
| `Ctrl-u` / `[` / `PgUp` | Half page up |
| `g` / `Home` | Jump to first entry |
| `G` / `End` | Jump to last entry, enter tail mode |
| `Enter` | Open entry detail overlay |
| `v` / `m` | Toggle Preview / Raw display mode |
| `w` | Toggle truncate / wrap |
| `d` | Toggle inline structured fields |
| `\` | Toggle sidebar visibility |
| `f` | Enter filter input mode |
| `/` | Enter search input mode |
| `n` / `N` | Next / previous search match |
| `1`–`9` | Toggle source N |
| `a` | Show all sources (clear `hidden_sources`) |
| `y` | Copy focused entry to clipboard (OSC 52) |
| `c` | Open copy submenu (`v` viewport, `s` stream, `a` all) |
| `e` | Export visible log to file |

**Sidebar focused**

| Key | Action |
|-----|--------|
| `Down` / `Up` | Move selection (drives focus filter) |
| `Enter` | Toggle source visibility (task entries); open process detail (process entries) |
| `Space` | Toggle source visibility |
| `s` | SIGTERM selected process |
| `S` | SIGHUP selected process |
| `a` | Show all sources |
| `1`–`9` | Toggle source N |

**Kill submenu** (under `k`)

| Key | Action |
|-----|--------|
| `k` | SIGTERM the focused task (so `kk` is "kill this") |
| `9` | SIGKILL the focused task |
| `a` | SIGTERM all direct children of root (root stays alive) |
| any other | Dismiss |

**Copy submenu** (under `c`), **Filter input** (`f` / `Esc` / `Enter` / history with `Up`/`Down`), **Search input** (`/` / `Enter` / `Esc`), **Entry detail** (`Esc`/`q` to close, `j`/`k` to scroll, `n`/`N` to step matches), **Process detail** (`Esc`/`q` to close, `s`/`S` for signal), **Quit confirmation** (`Enter` to confirm, anything else to dismiss).

The current keybinding scheme mostly mirrors Vim motions plus ad-hoc letters. A redesign pass is planned (see `open_issues.md`); the multi-task plumbing was deliberately shipped first.

### Tail Mode vs Pinned Mode

**Tail mode** (default while logs are flowing): new entries push the view to follow them. Status: `TAIL`. Any upward scroll (j/k up, Ctrl-u, PgUp) pins the view.

**Pinned mode**: view stays put. New entries accumulate. Status: `LINE 4821 (+47 new)` — current cursor entry plus how many new entries arrived since pinning. `G` / `End` jumps to the latest and re-enters tail mode.

Filter and source-visibility changes recompute the visible entry list; the cursor stays on the same entry if it's still visible, otherwise jumps to the nearest one. New tasks starting don't change scroll mode.

## Log Display

Entries are the unit of navigation; `j`/`k` always moves one entry regardless of visual height.

**Preview mode** (default): structured columns — timestamp, level badge, source tag, message. Source tags render as `[N] label` (numbered prefix matches the sidebar; `1-9` keys map to the same numbering). Cargo diagnostics and panic backtraces use specialized parser output for color-coding.

**Raw mode**: original text, no column formatting. Useful for output that has its own formatting.

**Wrap toggle** (`w`): orthogonal to display mode. Truncated = one visual line per entry; wrapped = entries occupy as many lines as needed.

**Highlighting**: search matches are inline-highlighted (reverse video). The current `n`/`N` match gets a brighter highlight. Filter matches don't highlight inline — the filter just controls visibility.

**Source colors**: `theme::SourceColors` assigns from a distinguishable palette per session; consistent across sidebar status tag, log source column, and detail view.

## Filtering

`f` enters filter input mode. The expression parses live on every keystroke; the log view updates immediately. Parse errors render inline (red text after the cursor) without clearing the view — the last valid filter stays active. `Up`/`Down` cycle filter history (session-scoped).

Filter syntax (`FilterExpr`):

```
level:error
level:error AND source:api
level:error OR level:warn
NOT source:health-check
-source:health-check                  # prefix negation
message:"connection refused"
level:/err.*/                         # regex
status:>400
error.code:ECONNREFUSED               # dotted field paths
```

Source visibility (sidebar focus + `hidden_sources` toggles + `1-9` keys + `a` show-all) composes with the filter expression: a source must be both visible AND match the filter to appear.

## Search

`/` opens the search input. As the user types, search runs against the current visible entries; matches highlight inline. `Enter` confirms, `Esc` cancels. After confirming, `n`/`N` jump forward/backward through matches.

Search operates on the post-filter entries — filter first to narrow, then search within. The existing `Search` builder handles regex, case sensitivity, match ranges; the TUI compiles once and reuses across entries, computing match byte ranges per-entry at render time only for on-screen entries.

## Scrolling

### Anchor-Based Rendering

Rather than maintaining a global height index, the log viewer only computes layout for what's on screen.

1. **Anchor entry**: scroll state tracks one entry and its Y position in the viewport.
2. **Render outward**: from the anchor, compute heights and lay out upward and downward until the viewport fills. ~100–200 entries even on tall terminals.
3. **No precomputation**: heights computed on demand at render time; no global cache across frames.

**Operations**: `j`/`k` move the anchor by one entry. `Ctrl-d`/`Ctrl-u` walk forward/back summing heights until a viewport's worth is consumed. `g`/`G` set the anchor to first/last visible. Resize re-renders from the same anchor (heights reflow around it). Filter or source-visibility change rebuilds the visible list and clamps the anchor to the nearest still-visible entry. Search jumps set the anchor to the matched entry.

The scroll position indicator uses entry count (`LINE 4821`), not visual line count — more meaningful for a log viewer anyway.

### Render Layer Separation

- **Data layer**: `LogStore` holds entries, `FilterExpr` decides visibility, `Search` finds matches. Render-agnostic.
- **View model**: `AppState` maintains the visible entry list, anchor, and source visibility. Updated on engine events.
- **Render**: pure function of (anchor, visible entries, terminal size, display mode, search state). Target 16ms per frame; drop frames rather than queue.

## Process Management

### Lifecycle

Process states (`ProcessStatus`):

```
Running -> Done
        \-> Failed(i32)
        \-> Stopped (user-initiated)
```

The engine tracks every spawned process group, polls signal-0 every 250ms (`monitor_spawns`) to detect exit, and republishes the snapshot when status changes. A global reaper task (`src/process.rs`) wait()s on each `tokio::process::Child` so zombies don't accumulate even when task code never awaits its handles.

### Controls

From the sidebar (focused on a process entry):

- `s` — SIGTERM (graceful)
- `S` — SIGHUP (reload semantics)
- `Enter` — open process detail panel (PID, PGID, command, status; future: live `lsof` for open sockets, controls)

Killing a single process via the sidebar's `s` does not stop the parent task — the engine's `kill_process` only signals the process group, mirroring how a server might handle SIGTERM. Stopping the parent task itself uses the kill submenu (`k`).

### Crash Surfacing

When a process fails, its sidebar entry turns red with the exit code (`seed-data [FAIL:1]`). If the user is in tail mode and the source is visible, the failure output is naturally on screen. Otherwise, a transient notification appears at the top — non-modal, auto-dismisses.

## Export & Re-streaming

### Export Visible Log

`e` (or `c v` viewport / `c s` stream / `c a` all from the copy submenu) writes the current visible entries to a file. Default filename: `runme-export-{timestamp}.log`. Respects current filter and source visibility — exports what you see.

### Copy to Clipboard

`y` copies the focused entry's raw text via OSC 52 (works over SSH). The `c` submenu offers viewport / stream / all variants.

## Event Loop Architecture

```
tokio::select! {
    event = terminal_events.next()          => handle_input(event, &mut app),
    Ok(entry) = log_rx.recv()                => app.ingest_entry(entry),
    Ok(_) = graph_rx.changed()               => app.refresh_from_graph(),
    _ = render_interval.tick(), if app.dirty => render(&app, &mut term); app.dirty = false,
}
```

Key points:

- **Engine-driven state**: graph and log streams are the inputs. `app` holds a snapshot of derived state (sidebar entries, focus filter, log lines) rebuilt on the appropriate engine signal.
- **Dirty-flag rendering**: only redraw on state change. Logs arriving faster than the frame rate set the flag once; the next tick picks it up.
- **Input never blocks**: terminal events are independent of log/graph ingestion. The user can scroll, filter, kill, or quit even when output is pouring in.
- **Backpressure**: `OutputBuffer` is a ring buffer, the broadcast channel drops old entries on lag. The TUI surfaces drops when it sees them.

## Open Questions

These are TUI-specific items that haven't earned a design pass yet:

1. **Sidebar redesign**: with multiple tasks each spawning multiple processes, the entry list gets dense. Collapsing / grouping / a separate "completed" section all need real-use validation before locking down a layout.
2. **Picker → argument form**: split layout for tasks that take args. Designed to fit; not built.
3. **Keybinding redesign**: current scheme is a flat per-mode match. LazyGit / LazyDocker / k9s patterns (numbered pane focus, multi-stage menus, footer hints, `?` cheatsheet) are inspirations. See `open_issues.md`.
4. **Theme/color configuration**: hardcoded dark theme today. Validate across terminal palettes; design a config system later.
5. **Mouse support**: scroll, click-to-select, click-sidebar-to-toggle. Significant QoL.
6. **Carriage-return progress output**: commands using `\r` to update a progress line in place produce garbled output today. See `open_issues.md`.
7. **Per-task UI mode**: `UiHint` lets tasks declare a preferred mode; could also let them declare a TUI quit preference (auto-close on success, stay open). Probably subsumed by the existing mode hint in practice.
