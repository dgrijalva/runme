# TUI Design

## Overview

The TUI is the primary interface for the power-user persona. It needs to handle two distinct phases: **activation** (picking what to run) and **interaction** (monitoring, controlling, and exploring running tasks and their output). The log viewer is the centerpiece — most time is spent reading logs, so that experience must be excellent.

## Existing Infrastructure

All discovered RUNME.rs files are compiled into a single binary via a generated Cargo workspace (see `system_design.md` § Multi-File Compilation). The TUI runs in-process with access to every task from every file via `Registry::from_inventory()`. Each task carries a `group` field identifying which RUNME.rs it came from.

The TUI builds on top of these existing primitives:

- **`LogEntry`** — universal log record with `raw`, `parsed` (Json/Logfmt/PlainText), `source`, `seq`, extracted fields (`timestamp`, `level`, `message`), and arbitrary `fields: HashMap<String, Value>`
- **`OutputBuffer`** — per-task ring buffer with `tokio::broadcast` for live subscription
- **`LogStore`** — multi-source composition layer. Merges entries across sources by `seq`, supports filtered composition, grouping by source/level/arbitrary key, and filtered live subscriptions
- **`FilterExpr`** — Lucene/Datadog-style filter DSL (`level:error AND source:auth`, negation, regex, wildcards, comparisons). Already has a parser (`winnow`-based) and evaluator
- **`Search`** — full-text search with match highlighting (byte ranges), regex, case sensitivity, and context windows with group merging
- **`ProcessHandle`** — spawn in process groups, signal (SIGTERM/SIGKILL/SIGHUP), graceful stop with escalation, `is_running()`, output buffer access
- **`TaskDef` / `Registry`** — task metadata (name, description, watch patterns, dependencies), lookup, sequential and parallel execution
- **`stream`** module — live tailing, filtered streaming, replay, export (raw text and JSON lines, sync and async)

## Layout

### Primary View: Log Viewer + Process Sidebar

```
+--[ processes ]--------+--[ logs ]----------------------------------------------+
| > api-server  [RUN]   | 12:01:03.412  INFO  api     Request handled GET /users  |
|   worker      [RUN]   | 12:01:03.415  DEBUG worker  Processing job 4821         |
|   migrate     [DONE]  | 12:01:03.418  ERROR api     Connection refused: db:5432 |
|   seed-data   [FAIL]  | 12:01:03.420  INFO  api     Retrying connection...      |
|                        | 12:01:03.421  WARN  worker  Queue depth > 100           |
|                        | 12:01:03.425  INFO  api     Connected to db:5432        |
|                        |                                                         |
|                        |                                                         |
|                        |                                                         |
|                        |                                                         |
+------------------------+---------------------------------------------------------+
| [filter: level:error ]                                               TAIL | 842 |
+----------------------------------------------------------------------------+-----+
```

**Process sidebar** (left):
- List of all tasks/processes with status indicators
- Status: `RUN`, `DONE` (exit 0), `FAIL` (non-zero exit/signal), `STOP` (user-stopped), `WAIT` (pending dependency)
- Color-coded: green for running, dim for done, red for failed
- Current selection highlighted — cursor moves with j/k
- Selecting a process scopes the log view to that source (or toggle to include/exclude from a multi-source view)
- Collapsible — toggle with a key to give the log viewer full width

**Log viewer** (main area):
- Occupies the majority of screen real estate
- Each line shows: timestamp, level (color-coded), source tag, message
- Structured entries show the extracted `message`; raw text entries show the raw line
- Source tags are short, color-coded labels (auto-assigned colors per source)
- Lines wrap or truncate (user preference; default truncate with horizontal scroll)

**Status bar** (bottom):
- Active filter expression (editable in-place)
- Scroll mode indicator: `TAIL` (auto-following) or `LINE 4821` (pinned position)
- Total entry count / filtered count
- Active search pattern if searching

### Detail View: Expanded Log Entry

When the user presses Enter on a log entry, an overlay or bottom pane expands to show the full structured content:

```
+--[ entry detail ]------------------------------------------------------+
| timestamp: 2024-01-15T12:01:03.418Z                                    |
| level:     error                                                        |
| source:    api-server                                                   |
| message:   Connection refused: db:5432                                  |
|                                                                         |
| service:    "api"                                                       |
| error.code: "ECONNREFUSED"                                              |
| error.host: "db"                                                        |
| error.port: 5432                                                        |
| trace_id:   "abc-123-def"                                               |
| span_id:    "0x7f3a"                                                    |
|                                                                         |
| --- raw ---                                                             |
| {"level":"error","msg":"Connection refused: db:5432","service":"api"..} |
+-------------------------------------------------------------------------+
```

- Shows all extracted fields, not just the summary line
- Flattened key paths for nested JSON (`error.code`, `error.host`)
- Raw text at the bottom for reference
- Scrollable if the entry has many fields
- Esc or q to close and return to log view

### Activation View: Task Picker

On startup (or when invoked with no specific task), the TUI shows a task picker that spans all discovered RUNME.rs files in the directory tree.

Because all RUNME.rs files are compiled into a single binary (see **Multi-File Compilation Model** above), the picker has immediate access to every task from every file via `Registry::from_inventory()`. No per-file compilation at picker time — that happened when the binary was built.

#### Grouping & Naming

Tasks are grouped by their `TaskDef.group` field — the relative path of the RUNME.rs file by default, or a human-friendly name if the file overrides it:

```
+--[ pick a task ]---------------------------------------------------+
| > web                                                               |
|                                                                     |
| .                                                                   |
|   start              Start all services                             |
|   clean              Clean all build artifacts                      |
|                                                                     |
| services/auth                                                       |
|   build              Build the auth service                         |
|   test               Run auth service tests                         |
|   migrate            Run database migrations                        |
|                                                                     |
| services/gateway                                                    |
|   build              Build the gateway                              |
|   start              Start the gateway in dev mode                  |
|                                                                     |
| web-app                                                             |
|   dev                Start webpack dev server                       |
|   build              Production build                               |
|   test               Run jest tests                                 |
|                                                                     |
+---------------------------------------------------------------------+
```

The root RUNME.rs group is shown as `.` (or the project name if one is set).

A RUNME.rs file can override its group name via a library API (TBD — something like `runme::set_name("Web Frontend")` in the file, or a macro attribute on main). This lets files self-describe with a human-friendly name while the path remains the default.

#### Interaction

- **Browse mode**: on launch, the full hierarchy is shown. Navigate with j/k, groups are collapsible with Enter or arrow keys.
- **Fuzzy search**: start typing to filter across all task names, descriptions, and group names. The hierarchy flattens into a ranked list as you type. Matching is across the fully qualified name (`services/auth > build`) so typing `auth build` finds it.
- **Enter** launches the selected task and transitions to the log viewer.
- **Multi-select** (stretch): select multiple tasks to launch in parallel (e.g. start the API server and the web dev server together). Toggle selection with Space, Enter to launch all selected.
- **Recent/frequent**: tasks you've run before surface higher in the fuzzy results. History is persisted across sessions (stored alongside the cache).

## App State Model

### Core State

```
AppState
  mode: AppMode              // which view is active
  processes: Vec<ProcessState>
  log_view: LogViewState
  filter: FilterState
  search: SearchState
  sidebar: SidebarState
```

### AppMode (what the user is doing right now)

```
enum AppMode {
    TaskPicker,         // fuzzy-find task selection
    Normal,             // log viewer, navigating with keyboard
    FilterInput,        // typing in the filter bar
    SearchInput,        // typing a search query
    EntryDetail,        // viewing expanded log entry
    CommandPalette,     // command palette overlay (future)
}
```

Modes are a stack — entering FilterInput pushes onto Normal, Esc pops back. This means the underlying view stays rendered while the input overlay is active.

### ProcessState

```
struct ProcessState {
    name: String,
    status: ProcessStatus,    // Running, Done, Failed(i32), Stopped, Waiting
    handle: Option<ProcessHandle>,
    visible: bool,            // whether this source appears in the log view
    color: Color,             // auto-assigned source color
}
```

### LogViewState

```
struct LogViewState {
    // The materialized, filtered, displayable list of entries
    visible_entries: Vec<DisplayEntry>,

    // Scroll position
    scroll: ScrollState,

    // Which sources are currently visible
    source_filter: HashSet<String>,  // empty = show all
}

struct DisplayEntry {
    entry: LogEntry,
    // Pre-computed: formatted timestamp, level badge, truncated message
    // These avoid re-computing on every frame
    formatted: FormattedLine,
}

enum ScrollState {
    Tail,                    // auto-scroll, showing latest entries
    Pinned { offset: usize, selected: usize },  // fixed position
}
```

### FilterState

```
struct FilterState {
    input: String,               // current text in the filter bar
    compiled: Option<FilterExpr>, // parsed filter (None if input is empty or invalid)
    error: Option<String>,        // parse error to display
    live: bool,                   // whether filter updates as you type (default: true)
}
```

### SearchState

```
struct SearchState {
    pattern: String,
    active: bool,                 // whether we're in search mode
    matches: Vec<usize>,          // indices into visible_entries that matched
    current_match: usize,         // which match we're focused on (for n/N navigation)
    highlight: bool,              // whether matches are highlighted in the log view
}
```

## Navigation & Interaction

### Keyboard Map

**Normal mode (log viewer)**

| Key | Action |
|-----|--------|
| `j` / `Down` | Move cursor down one entry |
| `k` / `Up` | Move cursor up one entry |
| `Ctrl-d` / `Page Down` | Scroll down half page |
| `Ctrl-u` / `Page Up` | Scroll up half page |
| `g` / `Home` | Jump to first entry |
| `G` / `End` | Jump to last entry, re-enter tail mode |
| `Enter` | Expand selected entry (detail view) |
| `f` | Focus filter input |
| `/` | Start search |
| `n` | Next search match |
| `N` | Previous search match |
| `Tab` | Toggle sidebar focus |
| `1-9` | Toggle visibility of source N |
| `a` | Show all sources |
| `q` | Quit |
| `?` | Help overlay |

**Sidebar focused**

| Key | Action |
|-----|--------|
| `j` / `k` | Move process selection |
| `Enter` / `Space` | Toggle source visibility in log view |
| `r` | Restart selected process |
| `s` | Stop selected process |
| `S` | Send SIGHUP to selected process |
| `Tab` | Return focus to log viewer |

**Filter input mode**

| Key | Action |
|-----|--------|
| typing | Updates filter expression, live-filters the log view |
| `Enter` | Confirm filter and return to normal mode |
| `Esc` | Cancel (revert to previous filter) and return to normal mode |
| `Ctrl-u` | Clear the filter input |
| `Up` / `Down` | Cycle through filter history |

**Search mode**

| Key | Action |
|-----|--------|
| typing | Updates search pattern |
| `Enter` | Confirm search, jump to first match, return to normal mode |
| `Esc` | Cancel search |

**Entry detail mode**

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll within the detail pane |
| `Esc` / `q` | Close detail, return to log view |
| `y` | Copy entry to clipboard (raw or formatted, TBD) |
| `n` / `N` | Close detail and jump to next/previous entry |

### Tail Mode vs Pinned Mode

This is critical for log viewer UX.

**Tail mode** (default when processes are running):
- New entries appear at the bottom, view auto-scrolls to follow
- The cursor stays at the bottom
- Visual indicator: `TAIL` in the status bar, possibly a subtle pulsing/color indicator
- Any scroll-up action (j with nothing below, k, Ctrl-u, Page Up, mouse scroll up) immediately switches to pinned mode

**Pinned mode** (the user is reading):
- View stays at the current scroll position
- New entries accumulate but don't push the view
- Visual indicator: `LINE 4821 (+47 new)` — shows current position and how many new entries have arrived since pinning
- The `(+N new)` counter is a key affordance — the user knows they're behind and can see how fast entries are arriving
- Pressing `G` (or `End`) jumps to the latest entry and re-enters tail mode
- Pressing `Shift-G` or a dedicated key could jump to the newest entry *without* re-entering tail mode (peek at latest, then return to reading position)

**Transition rules:**
- Start in tail mode
- Any upward scroll -> pinned
- `G` / `End` -> tail
- Changing filter -> stay in current mode but recompute visible entries from the new position
- New process starts -> stay in current mode

## Log Display

### Line Formatting

Each log entry is rendered as a single line (or wrapped, if the user enables wrapping). The format depends on what was parsed:

**Structured (JSON/logfmt) entries:**
```
12:01:03.418  ERROR  api     Connection refused: db:5432
^timestamp    ^level ^source ^message (extracted)
```

- Timestamp: extracted `timestamp` field, formatted as local time `HH:MM:SS.mmm`. If no timestamp, blank/dash.
- Level: color-coded badge. ERROR=red, WARN=yellow, INFO=green, DEBUG=dim. Fixed width for alignment.
- Source: short name, color-coded per source. Fixed width (truncated if long).
- Message: extracted `message` field. This is the primary content the user scans.

**Plain text entries:**
```
12:01:03.418  ---    api     Compiling runme v0.1.0
```
- No level extracted -> show `---` or blank in the level column
- Message is the raw text

**Cargo diagnostics, panic backtraces:**
- Use the specialized parser output to format these more readably
- Errors in red, warnings in yellow, note/help in blue
- Backtrace frames could be indented or collapsible (stretch goal)

### Column Alignment

Fixed-width columns for timestamp, level, and source create a scannable layout. The message column gets all remaining space. This is important: when scanning logs quickly, the eye needs to be able to find the message column at a consistent horizontal position.

If a field is missing (no timestamp, no level), the column stays blank to preserve alignment.

### Source Colors

Auto-assigned from a palette of distinguishable terminal colors. The color is consistent per source name within a session. The same color appears in:
- The source tag in log lines
- The process name in the sidebar
- The source indicator in the detail view

### Highlighting

Active search matches are highlighted inline (reverse video or bright color on the match range). The current match (the one `n`/`N` is focused on) gets a distinct highlight color (e.g. bright yellow vs dim yellow for other matches).

Filter matches don't highlight inline — the filter just controls which entries are visible.

## Filtering

### Live Filter

When the user presses `f` to enter filter mode:
1. The cursor moves to the filter bar at the bottom
2. As they type, the filter expression is parsed and applied **on every keystroke**
3. The log view updates live, showing only matching entries
4. Parse errors are shown inline (red text after the cursor) but don't clear the view — the last valid filter stays active

This means the filter engine needs to be fast. The existing `FilterExpr` evaluator runs per-entry, so the bottleneck is recomputing the visible entry list. Approaches for performance:

- **Incremental filtering**: when the filter changes, don't re-scan the entire LogStore. Instead, maintain a materialized view that can be incrementally updated.
- **Debounce**: if keystroke rate exceeds filter computation rate, debounce to avoid falling behind. But aim for <16ms per filter pass so debouncing isn't needed for reasonable buffer sizes.
- **Background computation**: filter recomputation happens on a background task. The UI thread never blocks on filtering. Stale-but-visible is better than frozen.

### Filter Syntax

The existing `FilterExpr` DSL supports:

```
level:error                          # field match
level:error AND source:api           # boolean AND
level:error OR level:warn            # boolean OR
NOT source:health-check              # negation
-source:health-check                 # prefix negation (same as NOT)
message:"connection refused"         # quoted exact match
level:/err.*/                        # regex
status:>400                          # numeric comparison
error.code:ECONNREFUSED              # dotted field paths
```

This should work well for the TUI. The filter bar should show a brief syntax hint when empty (placeholder text like `filter: level:error AND source:api ...`).

### Source Toggle Shorthand

In addition to the filter DSL, the sidebar's source visibility toggles provide a quick way to include/exclude sources. These compose with the filter: a source must be both visible (sidebar) AND match the filter to appear.

Toggle interaction:
- In the sidebar, `Enter`/`Space` on a process toggles its `visible` flag
- Number keys `1-9` toggle source N from anywhere (mapped to sidebar order)
- `a` shows all sources
- The filter bar shows active source toggles as a visual indicator (dimmed names in the sidebar for excluded sources)

## Search

### Interaction

`/` opens the search input. As the user types, the search is executed against the current visible entries. Matches are highlighted in the log view. Enter confirms; Esc cancels.

After confirming, `n` jumps forward to the next match, `N` jumps backward. The status bar shows `[match 3/17]` or similar.

Search operates on the *visible* (post-filter) entries. This is intentional — you filter first to narrow down, then search within the result.

### Implementation

The existing `Search` builder handles text search, regex, case sensitivity, and match range extraction. For the TUI, we need:

- Compile the search once, then reuse across entries
- Apply incrementally as new entries arrive (in tail mode, new entries should be checked against the active search)
- Store match ranges on `DisplayEntry` so the renderer can highlight without re-searching

## Scrolling

### The Problem

Terminal UIs have a reputation for janky scrolling. The root causes are usually:
1. Re-rendering the entire screen on every scroll event
2. Slow content computation (formatting, filtering) blocking the render loop
3. No concept of viewport — treating the log as a single giant widget

### Approach

**Viewport-based rendering**: only compute and render the lines currently visible in the viewport (plus a small buffer above and below for smooth scroll). This is similar to virtual scrolling in web UIs.

```
struct Viewport {
    // The full list of entry indices (into the LogStore/filtered view)
    total_entries: usize,

    // Current top-of-viewport position
    offset: usize,

    // How many lines fit in the viewport
    height: usize,

    // Pre-rendered lines for the visible range + buffer
    // This avoids re-formatting on every frame
    rendered_cache: Vec<RenderedLine>,
    cache_range: Range<usize>,  // which entry range the cache covers
}
```

**Separation of concerns**:
- **Data layer**: `LogStore` holds entries, `FilterExpr` determines visibility, `Search` finds matches. None of these know about rendering.
- **View model layer**: `LogViewState` maintains the filtered/sorted entry list, scroll position, and display metadata. Updated asynchronously when new entries arrive or filters change.
- **Render layer**: takes the viewport slice from the view model and draws it. Pure function of (viewport_entries, terminal_size, theme). Fast enough to run at 60fps.

**Smooth scrolling signals**:
- Keyboard repeat rate drives scroll speed naturally
- Consider scroll acceleration: holding j/k for extended time could increase step size (1 -> 5 -> 20 lines)
- Mouse scroll events map to 3-line jumps (configurable)
- Page up/down moves by `viewport_height - 2` (overlap for context)

**Frame budget**: target 16ms per frame (60fps). If rendering takes longer, drop frames rather than queuing. The user should never see input lag.

## Process Management

### Lifecycle

Each process has a state machine:

```
Waiting -> Running -> Done
                  \-> Failed(exit_code)
                  \-> Stopped (user-initiated)
```

`Waiting` is for tasks with unmet dependencies (not yet implemented but the slot exists in the model).

### Controls

From the sidebar:
- **Stop** (`s`): sends `ProcessHandle::stop()` (SIGTERM -> wait -> SIGKILL)
- **Restart** (`r`): stop then re-launch the same command
- **Signal** (`S`): sends SIGHUP for reload semantics
- **Kill** (stretch: `K`): immediate SIGKILL, no grace period

The restart action preserves the existing log buffer and appends new output. A visual separator (`--- restarted at 12:05:03 ---`) marks the boundary.

### Crash Surfacing

When a process fails:
- Its status in the sidebar turns red: `seed-data [FAIL:1]` (showing exit code)
- If the log view is in tail mode and includes this source, the last few lines (including any error output) are visible naturally
- If the source is filtered out or the user is scrolled up, a transient notification appears: `seed-data exited with code 1` — shown briefly at the top or bottom of the log view, non-modal, auto-dismisses after a few seconds (or on any keypress)

## Export & Re-streaming

### Dump to File

From normal mode or entry detail:
- A command (`:export` or a keybinding) writes the current visible log view to a file
- Options: raw text, JSON lines, or formatted (with colors stripped)
- Default filename: `runme-export-{timestamp}.log`
- Respects current filter and source visibility — exports what you see

### Copy to Clipboard

- In entry detail: `y` copies the full entry (raw or formatted)
- In normal mode with a selection: `y` copies the selected line(s)
- Clipboard integration via OSC 52 escape sequence (works over SSH) or platform-specific fallback

### Pipe to External Command

Stretch goal: select a source and pipe its live output to an external command. Uses the existing `stream::tail_source()` infrastructure. The TUI would spawn a child process and connect a filtered broadcast receiver to its stdin.

## Event Loop Architecture

```
tokio::select! {
    // Terminal input events (keyboard, mouse, resize)
    event = terminal_events.next() => {
        handle_input(event, &mut app_state);
    }

    // New log entry from any running process
    entry = log_receiver.recv() => {
        ingest_entry(entry, &mut app_state);
    }

    // Process status change (exited, crashed)
    status = process_watcher.next() => {
        update_process_status(status, &mut app_state);
    }

    // Render tick (if state has changed)
    _ = render_interval.tick(), if app_state.dirty => {
        render(&app_state, &mut terminal);
        app_state.dirty = false;
    }
}
```

Key design points:
- **Dirty flag rendering**: only re-render when state has changed. New log entries set the dirty flag; the render tick picks it up. This prevents redundant renders when entries arrive faster than the frame rate.
- **Input is never blocked**: terminal event handling runs independently of log ingestion. The user can always scroll, filter, or quit, even if logs are arriving at high volume.
- **Backpressure**: if log entries arrive faster than the view can ingest them, the broadcast channel's lagged behavior drops old entries. The ring buffer in `OutputBuffer` provides the same safety net. The TUI should detect and surface lag: `(N entries dropped)`.

## Open Questions

These are things to figure out during implementation, not blockers:

1. **Sidebar width**: fixed vs auto-sized to longest task name? Resizable by the user?
2. **Multi-line log entries**: some structured logs (stack traces, multi-line JSON) span multiple terminal lines. Collapse to one line in the main view, expand in detail? Or show multi-line inline with indentation?
3. **Mouse support**: click to select entry, click sidebar to toggle source, scroll wheel. Worth doing early or defer?
4. **Horizontal scrolling**: for very long lines that exceed terminal width. Vim-style (`h`/`l` to scroll) or auto-wrap toggle?
5. **Theme/color configuration**: hardcode a sensible dark theme first, make configurable later?
6. **Startup with no tasks**: if RUNME.rs defines tasks but none are running yet, should the TUI show the task picker immediately, or show an empty log view with the sidebar listing available (but not running) tasks?
7. **Split views**: show two sources side-by-side instead of interleaved? Useful for comparing, but adds layout complexity. Defer to a later iteration?

## Implementation Order

Suggested sequence for incremental buildout:

1. **App shell**: ratatui setup, event loop, terminal init/restore, quit handling. Empty screen with a status bar.
2. **Process sidebar**: render task list with status, handle process lifecycle events. No log view yet.
3. **Log viewer (basic)**: render entries from OutputBuffer, tail mode, basic scrolling (j/k, Ctrl-d/u, G).
4. **Scroll refinement**: pinned vs tail mode, `+N new` indicator, smooth scrolling, viewport-based rendering.
5. **Source filtering**: sidebar toggles, source colors, multi-source interleaving.
6. **Filter bar**: live filtering with the existing `FilterExpr` engine, filter input mode.
7. **Search**: `/` search, `n`/`N` navigation, match highlighting.
8. **Entry detail**: expand a log entry to see all fields, raw text.
9. **Process controls**: stop, restart, signal from sidebar.
10. **Export**: dump to file, clipboard copy.
11. **Task picker**: fuzzy-find activation UI.
