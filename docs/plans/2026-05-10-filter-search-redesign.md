# Filter / Search UX redesign

**Date:** 2026-05-10
**Status:** Design closed, ready for implementation
**Scope:** TUI filter input and search input. Source-list UX and the broader keybinding/discoverability rework are out of scope and deferred.

## Why

The expression filter input has accumulated UX problems:

- The active-filter indicator in the status bar is too easy to miss.
- Clearing an active filter requires `f` → `Ctrl-u` → `Enter` — three keys for a should-be-trivial operation.
- The modal `[Enter] save / [Esc] revert` ceremony reads heavier than it is, because the filter live-parses as you type — Enter is mostly theater.
- Filter history (`Up` / `Down` inside the input) is a bug farm and not discoverable.

Search (`/`) has the same modal shape and largely the same issues. The two should be redesigned as a coherent pair — same chrome, same history behavior, different output bindings. The source-list mechanisms and the wider keybinding scheme are not touched in this pass.

## Decisions

### Commit model — unchanged

Live-apply with snapshot-revert stays. Typing updates the input and live-parses; the last successfully-parsed expression is the active filter. The text active when the panel was opened is snapshotted so `Esc` can revert. `Enter` closes the panel and commits the current text to history. The reason this stays is that the visible behavior is right; what was wrong was the *visibility* of the panel itself, not the model.

### Chrome — bordered box, full-width, three rows

The filter and search inputs render as a full-width bordered box with rounded box-drawing characters. Three rows:

```
╭─ filter ────────────────────────────────────────╮
│ level:error AND source:api                      │
╰─ [enter] save  [esc] cancel ────────────────────╯
```

- The top edge carries the title (`filter` or `search`).
- The middle row is the input itself, with cursor.
- The bottom edge carries a minimal hint string: `[enter] save  [esc] cancel`. Ctrl-u (clear) and Up/Down (history) are intentionally omitted from the hint string — they are not "primary" actions and can live in muscle memory or a future `?`-overlay.

The control is always focused while visible — no idle state to design for. Filter and search use the exact same chrome; only the title text and (eventually) the hint contents differ. Parse errors continue to render after the input text on the middle row.

### Status-bar indicator — chip styling

The `filter: <text>` span in the status bar (and the equivalent search span) renders as a solid-bg chip, matching the existing ` runme ` brand chip: `fg(Color::Black).bg(THEME.accent)`. The chip stays in its current position in the status bar — no new screen real estate. The contrast is what makes it pop.

### Match highlighting in the log viewer

When a filter is active, the substrings that contributed to a positive value match are highlighted in the rendered log, the same way search already highlights its pattern.

- Only **positive** value matches highlight. For `level:error AND timeout`, both `error` (the matched value of the `level` field) and `timeout` (a bare literal) highlight in the entries that contain them.
- Negations and field names do not highlight. `NOT foo` highlights nothing for `foo`.
- The highlight color is **different from search** so the two can be visually distinguished when both are active simultaneously. Specific palette entry: `THEME.filter_match_bg` / `THEME.filter_match_fg` (to be added).
- Filter highlights apply to entries that are already visible — there is no attempt to surface matches that the filter has filtered out, or that are off-screen due to wrap. (Same caveat already applies to search.)

### Clearing — Esc from normal mode

Pressing `Esc` from normal mode clears both the active filter expression and the active search in a single keystroke. Source hides and focus filter are *not* touched by this Esc — they have their own clearing affordances and are out of scope for this pass.

Inside the filter / search input panels, `Esc` keeps its existing meaning: revert to the saved snapshot and close the panel.

### History — virtual-slot model

Each input panel has a history of committed entries plus a single mutable "virtual" slot that represents the user's current edit. The position sequence during a session is:

```
[saved_oldest, ..., saved_newest, virtual, blank]
```

- **On entering the panel**: virtual ← the currently-active text (filter expression or search pattern). Position = virtual. Cursor goes at the end of the text.
- **Saved entries are read-only.** Up walks the position one step toward `saved_oldest` and clamps there; Down walks one step toward `blank` and clamps there.
- **Any typing operation** (insert, backspace, Ctrl-u) targets the virtual slot. If the position was on a saved entry or on blank, the position snaps to virtual first, and the edit is applied. Virtual is the only mutable slot.
- **Virtual persists across navigation within the session.** If you Up into history then Down back, the virtual value you last edited is still there.
- **Virtual is freely overwritten** when typing from blank (e.g., typing `bar` while the position is at blank moves you to virtual and sets virtual = `bar`, overwriting whatever was there).

Worked examples (showing only the final input text):

```
f 'foo' down up                 → 'foo'    (virtual='foo', Down to blank, Up back to virtual)
f 'foo' down 'bar' up           → 'bar'    (typing at blank overwrites virtual; no saved to step into)
f 'foo' enter f 'd'             → 'food'   (after enter, saved=['foo']; reopen, virtual='foo', type 'd')
f 'foo' enter f 'd' up          → 'foo'    (from virtual='food', Up steps into saved[0]='foo')
```

### Commit semantics with MRU dedup

On `Enter`:

- If the committed text is empty, history is untouched. (Empty is not a useful history entry.)
- Otherwise, remove any existing exact-match occurrence of that text from saved history, then push the text to newest. This subsumes the current "skip if equal to last" check — that's just the special case where the duplicate already happens to be newest. It also handles the case where the user `Up`'s into an older entry and presses `Enter` to re-apply it: the entry gets bumped to the top of history.

Edited versions of a saved entry are distinct strings and do not dedup. `saved=['foo','bar']`, then `Up` to `foo`, edit to `foobaz`, `Enter` → `saved=['bar','foo','foobaz']`.

### Code structure — shared input control

Filter and search share a single text-input type. They are essentially the same widget bound to different output sinks (one parses to `FilterExpr`, the other becomes a search pattern). The shared control owns:

- `text`, `cursor`
- `saved_text` (snapshot for revert)
- `history: Vec<String>` (saved entries)
- `virtual_text: String` (the editable slot's value when position is not at virtual)
- `history_pos: HistoryPos` (enum: `Saved(usize)`, `Virtual`, `Blank`)

Methods covered: `insert_char`, `delete_char_before`, `move_left`, `move_right`, `clear`, `set_text`, `save_current`, `revert`, `history_up`, `history_down`, `commit_to_history`, plus a shared renderer for the bordered chrome.

`FilterInputState` becomes a thin wrapper that owns a `TextInput` plus the filter-specific parse cache (`last_valid_expr`, `parse_error`). `SearchState` similarly wraps a `TextInput` plus its match-tracking fields (`active`, `pattern`, `match_indices`, `current_match`). Filter history previously lived on `AppState` (`filter_history`, `filter_history_index`); both fields move into the embedded `TextInput`.

## Out of scope (deliberately)

- Source-list UX (sidebar selection, source toggles, `a`-to-show-all, the `N hidden` badge).
- The wider keybinding rework (leader keys, which-key overlay, footer hints, numbered panes, `?` cheatsheet). Tracked separately in `docs/open_issues.md`.
- Search getting a separate `Highlight match colors`-style settings UI. Search keeps its current single highlight style.
- Idle / unfocused styling of the input chrome. The control is only rendered while focused.
