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

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:

- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line
