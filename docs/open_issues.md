# Open Issues

## ~~exec'd processes should appear in the sidebar~~ (RESOLVED)

**Resolved:** `ctx.exec()` is now sugar for `ctx.spawn(cmd).complete().await`. Every exec'd process goes through the `SpawnBuilder`, emits a `SpawnEvent`, and appears in the TUI sidebar while running. Commit `f7660db`.

## ~~Crate naming — `runme` is taken on crates.io~~ (RESOLVED)

**Resolved:** Renamed to `rnme`. Library and CLI binary are a single merged crate — `cargo install rnme` gives you the binary, `use rnme::prelude::*` gives you the library. RUNME.rs filename convention kept as-is for readability.

## TUI keybinding layout needs a rework

**18:43** — Current shortcut scheme isn't satisfying. Want to study LazyGit (and a bit of Vim) for inspiration on key layouts appropriate for a system like this.

- *Effort:* Moderate — keybinding plumbing already centralized in `src/tui/keys.rs`, but a real redesign means rethinking modes, discoverability, and possibly conventions across the whole TUI
- *Assessment:* Current scheme (in `src/tui/keys.rs`) is a flat per-mode `match` on `KeyCode`. It mostly mirrors Vim motions (`j`/`k`/`g`/`G`/`Ctrl-d`/`Ctrl-u`/`/`/`n`/`N`) plus ad-hoc single-letter actions (`s`/`S` signals, `a` show-all, `f` filter, `c` copy menu, `e` export, `y` yank, `w` wrap, `d` fields, `v`/`m` raw, `\` sidebar, `1-9` source toggles, Enter detail). No leader key, no which-key/help overlay, no consistent pane-numbering nav, and `1-9` overloads source toggles in a way that won't scale beyond 9 sources. Worth pulling apart what LazyGit does well: numbered pane focus, context-sensitive footer hints, multi-stage menus (e.g. `c` → commit submenu), and a global `?` cheatsheet
- *Concern:* Affects muscle memory — once a scheme is published, churn is annoying. Pre-release status (per CLAUDE.md) means now is the right window. Also: a redesign should probably be paired with a discoverability mechanism (footer hints / `?` overlay), otherwise users won't find the new bindings
- *Inspiration sources to look at:* LazyGit (panes + which-key menus + footer hints), LazyDocker (similar TUI shape, log-heavy), k9s (resource navigation), Vim/Helix (motion + text-object grammar). Helix may be more relevant than Vim proper since its keybinding philosophy is closer to a curated app than a programmable editor

## Picker becomes the "new task" menu — multi-task TUI model

**18:56** — Originally framed as "back to root menu kills current task and reopens picker". Reframed (**19:08**) as something broader and more interesting: turn the picker into a persistent *new task* menu, accessible at any time.

**The vision:**
- Launching `runme` with no args opens the new-task menu (today's picker), same as before
- From any running task, a key opens the new-task menu — picking a task **spawns** it alongside the current one rather than replacing it
- "Terminate this task" and "Quit runme" become separate, distinct actions (today they're effectively the same — `q` quits everything)
- When the last running task terminates, the new-task menu opens automatically. So "back to picker" falls out naturally: terminate the current task, picker reappears

**Why this is the better framing:** runme already has the building blocks for tracking, starting, and stopping multiple tasks — they're just not exposed in the TUI's one-task-at-a-time shell. Unifying them turns runme into a small task supervisor instead of a one-shot launcher, and the "menu" becomes a first-class, always-available concept rather than a startup-only screen.

- *Effort:* Major refactor — but most of the engine work is already done
- *Assessment — what's already in place:*
    - `TaskRunner` (runner.rs:39) is **already multi-session**: `sessions: Vec<TaskSession>`, `launch()` is additive and returns a session ID, `shutdown()` iterates all executions. The doc comment even says "manages one or more `TaskExecution`s". The TUI just never calls `launch()` more than once
    - Each session owns its own `TaskStatus` + `processes: Vec<ProcessInfo>` Arcs. The shared `LogStore` already aggregates entries from multiple sources, so multi-task log display works for free
    - `runner.shutdown(timeout)` already does graceful shutdown across all executions
- *Assessment — what needs to change:*
    1. **AppState**: drop the singletons (`task_status`, `task_name`, `processes`, `tui_wait`, `tui_output`) and read per-session state from `runner.sessions` instead. The "first session backward-compat fields" on `TaskRunner` (runner.rs:114-118) become dead code
    2. **Picker as overlay mode**: today `AppMode::TaskPicker` is full-screen and mutually exclusive with `Normal`. Either make it an overlay over Normal (like `EntryDetail` / `CopyMenu` / `ProcessDetail`) or keep it full-screen but allow re-entry from Normal. Overlay is probably cleaner — sidebar context stays visible behind it
    3. **Sidebar**: today shows one task with its processes nested under it. Needs to handle N tasks, each with its own process list. Probably: top level is sessions, expandable to show their processes. The depth/grouping in `SidebarEntry` already supports this
    4. **Task lifecycle for "terminate this task"**: per-session shutdown — call `exec.shutdown(timeout)` on the relevant execution, mark the session as ended, leave its log entries in the store. Currently `TaskExecution::shutdown` exists but isn't exposed per-session through the runner
    5. **Auto-open menu on last-task-end**: simple condition in the event loop — when all sessions transition out of running state, switch to picker overlay. Need to decide whether completed sessions stay visible in the sidebar (probably yes, with a "Done" marker — keeps the log accessible)
    6. **Quit vs terminate split**: `q` in Normal currently sets `running = false`. Reassign: `q` becomes "terminate this task" (the focused one in the sidebar), and quitting runme becomes a separate keybind (`Ctrl-q` is the obvious choice, or a prompt — "kill 3 running tasks?")
    7. **Log filtering / source identity**: with multiple tasks, logs from different tasks need to be visually distinguishable. Source colors already exist; sidebar source filtering already exists. Worth checking that two tasks spawning the same `cargo build` source string don't collide — may need to namespace sources by session ID
- *Concern:* Resource use grows linearly with task count. No protection today against a user spawning 50 long-running tasks. Probably fine for now (user-driven, foot-gun is acceptable) but worth a backstop later
- *Concern:* Readiness conditions (`ready_on_port`, etc.) are per-process, not per-task-runner. Multi-task should "just work" but worth verifying that two tasks watching different ports don't interfere
- *Concern:* TUI hooks (`tui_wait`, `tui_output`) are shared across all executions in the runner today. With multi-task, "should the TUI stay open after this task ends?" is a per-task question, not a runner-wide one. Probably resolves to: TUI stays open as long as **any** task is running, otherwise opens the menu (per the auto-open behavior)
- *UX note:* Pairs naturally with the keybinding redesign — `n` for new task / `t` for terminate this task / `Ctrl-q` to quit runme are all reasonable, but the whole layout deserves to be designed coherently rather than piecemeal

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:
- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line

