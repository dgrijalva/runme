# TTY special-character handling

**Status:** Partially landed — display sanitization in TUI render. Full collapse-based model deferred.
**Scope:** How rnme should treat control characters (`\r`, ANSI escapes, etc.) that appear in process output captured for logs.

## The problem

Programs that detect a TTY and decide to emit progress UIs include `aws s3 cp`, `curl`, `wget`, `pip install`, `npm install`, `cargo build`, `docker pull`, and many others. When rnme captures their output through pipes, the program is supposed to fall back to plain-text output — but several behaviours leak through anyway:

- **`\r` carriage return.** Programs use it to update a single progress line in place. Some keep emitting it even when stdout is a pipe.
- **`\x1b[...` ANSI escapes.** Color codes (e.g. `cargo`, `eslint`), cursor moves, line-clear sequences. Many tools emit these unconditionally.
- **Other C0 controls** — backspace (`\x08`), bell (`\x07`), form feed (`\x0c`).

In the TUI log viewer, these characters are passed through `entry.raw` / `entry.message` directly into ratatui line spans. The terminal hosting the TUI then interprets them, producing visible corruption: the cursor jumps mid-line, characters disappear, layout breaks.

The original symptom captured in `docs/open_issues.md`:

> Commands that use `\r` to update a progress line in-place produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

## What landed

A display-layer sanitization pass in `src/tui/render.rs` strips control characters and ANSI escape sequences from text before handing it to ratatui. Tabs and newlines are preserved (newline because wrapped-mode rendering splits multi-line records on `\n`). The underlying `LogEntry.raw` is unchanged — only the rendered output is sanitized.

This makes the TUI display safe to look at. It does not address:

- Storage bloat (every progress tick is still a separate `LogEntry`).
- MCP `get_logs` / report output (still emits the raw bytes verbatim).
- CLI stdio output (still prints `\r` and ANSI through to the user's terminal, which is sometimes desirable — see below).
- The "source column shows truncated/corrupted text" issue from the original note — that's a separate problem about command-string-as-source-name.

## The fuller model we considered (deferred)

The TUI display sanitization is a cosmetic fix. A more complete solution would treat `\r`-delimited progress as a first-class concept end-to-end. Sketch of the model:

### Parser

Only `PlainLineParser` would learn about `\r`. Other parsers (JSON, logfmt, cargo_diag, rust_panic) keep splitting on `\n`. The parser emits records with a new `transient: bool` flag on `RawRecord`.

Parser-level edge cases:

| Input | Today | Proposed |
|---|---|---|
| `foo\n` | permanent | permanent (unchanged) |
| `foo\r\n` (CRLF) | permanent with stray `\r` in raw | permanent, `\r` stripped |
| `foo\rbar\n` | one permanent with embedded `\r` | transient(`foo`) + permanent(`bar`) |
| `foo\rbar\r` | one record so far (no `\n`) | transient(`foo`) + transient(`bar`) |
| `foo\r` (no more data, not EOF) | Incomplete | Incomplete (could be CRLF — wait) |
| `foo\r` (EOF) | emit as permanent at EOF | emit as transient |
| `\r\r\rfoo\n` | one record `\r\r\rfoo` | either skip empty transients or emit them — undecided |

The `\r\n` rule matters: Windows tooling routinely emits CRLF.

### Store

`LogStore.push` collapses adjacent transients from the same `(source, stream)` pair. Stream scoping matters: stdout and stderr from the same process should maintain independent transient chains.

Two policy options when a permanent arrives after a run of transients:

- **(a) Sticky tail** — preserve the last transient. Sequence `P10%\r P50%\r P100%\r Done\n` ends up as: transient(`P100%`) + permanent(`Done`). You see what the progress reached.
- **(b) Drop on promotion** — drop preceding transient(s) when a permanent arrives from the same source/stream. Same input → permanent(`Done`) only.

(a) is more informative; (b) is cleaner. No decision made.

### Broadcast

Two options for telling subscribers about collapse:

- **Pass-through:** broadcast still carries `LogEntry`, with `transient: bool` on the entry. Each subscriber that keeps its own copy of the store (TUI, MCP forwarder) re-applies the same collapse rule locally.
- **Event-typed:** change the broadcast item to `enum LogEvent { Push(LogEntry), Replace { prior_seq, new: LogEntry } }`.

Pass-through is lower blast radius; the rule is small enough to copy.

### TUI viewport

Viewport pins by `seq`. When a transient is collapsed away:

- `Tail` mode: invisible — cursor is always at end.
- `Pinned` mode with cursor on the collapsed transient: `resolve_seq` snaps to the next-larger surviving seq → cursor visibly jumps.
- `Pinned` mode with cursor elsewhere: unaffected.

The cursor-jump case is real but probably acceptable since transients are inherently unstable anchors. An alternative is "never let the user pin a transient — auto-advance to the next permanent" but that adds state.

### MCP / query surface

- `get_range`, `grep`, `subscribe_with`: all operate over `LogStore.sources[id]`. Already collapsed → results are clean.
- `since_seq` pagination: if seqs 5, 6, 7 were all transients and got collapsed to 7, a client paging from `since=4` sees seq 7 directly — gap 5→7. Gaps already exist today via capacity eviction, so callers can't rely on contiguity. Should be documented.

## Open questions if/when we revisit the full model

1. Promotion policy — sticky tail (a) vs drop on promotion (b)?
2. `\r\r\r` empty runs — emit empty transients, or skip?
3. Cursor on collapsed transient — let it snap, or auto-advance?
4. CLI stdio behaviour — when a transient arrives, should the CLI also emit `\r` to overwrite the previous line in its own terminal, or print everything? (CLI is a separate subscriber from TUI; its UX choice is independent.)
5. `OutputBuffer` collapse — only at `LogStore`, or also in per-process `OutputBuffer`s? Matters if anything reads `OutputBuffer` directly without going through `LogStore`.

## Other ANSI / terminal control sequences to consider

The current display sanitizer strips everything in the C0/C1/DEL ranges plus CSI and OSC sequences. The list below is what to remember if we ever do a richer pass:

### Cursor / line manipulation (CSI)

- `\x1b[2K` — erase entire line. Used by multi-line progress UIs (`docker pull`, `pip install`) to refresh a status line.
- `\x1b[K` / `\x1b[0K` / `\x1b[1K` — erase to end / to start / entire line.
- `\x1b[NA` / `\x1b[NB` — move cursor up / down N rows. Multi-line progress (`docker pull` with layered downloads, `cargo build` "Compiling..." spinners) uses this to overwrite the rectangle above.
- `\x1b[NC` / `\x1b[ND` — move cursor right / left.
- `\x1b[N;Mf` / `\x1b[N;MH` — cursor position absolute.
- `\x1b[s` / `\x1b[u` — save / restore cursor.
- `\x1b[?25l` / `\x1b[?25h` — hide / show cursor.
- `\x1b[2J` — erase entire screen (full-screen TUI tools).

### Color / styling (SGR)

- `\x1b[Nm` — foreground/background color, bold, italic, underline, etc. (`\x1b[31m` = red, `\x1b[0m` = reset, plus 256-color and truecolor variants).
- These are the most common escapes by volume. The current sanitizer strips them. A more sophisticated pass could translate them into ratatui spans with matching colors.

### Operating System Commands (OSC)

- `\x1b]0;title\x07` or `\x1b]0;title\x1b\\` — set terminal window title.
- `\x1b]8;;url\x1b\\text\x1b]8;;\x1b\\` — clickable hyperlinks (used by `cargo`, `rustc`, modern `ls`).
- `\x1b]10`/`11` — query/set fg/bg colors.

### Other C0 controls

- `\r` (0x0D) — carriage return. Primary culprit.
- `\x08` (0x08) — backspace. Used by progress UIs that count up character-by-character.
- `\x07` (0x07) — bell. Programs use it for completion notification.
- `\x0c` (0x0C) — form feed. Used by some tools to "clear" output.
- `\x09` (0x09) — tab. Preserved (legitimate text).
- `\x0a` (0x0A) — line feed. Preserved.

### Hopeful note

Most well-behaved tools detect that stdout is not a TTY (via `isatty(2)`) and disable progress UIs and colors automatically. The leakage we're cleaning up represents tools that either:

1. Don't detect TTY (lazy or buggy).
2. Intentionally force colors / progress on (e.g., `cargo --color=always`, `npm install --color=always`, `FORCE_COLOR=1`).
3. Use stderr for progress while stdout is detected as non-TTY (common pattern that fools simple TTY detection).

For case (2) and (3) we'd ideally translate colors into ratatui styling rather than strip them. That's a real feature, not just hygiene. Out of scope for the minimal fix.

## File pointers

- Parser chain: `src/log/parse/mod.rs:161` (`default_parser`)
- Plain line parser: `src/log/parse/plain.rs`
- Per-process buffer push: `src/log/buffer.rs:49`
- Canonical store push: `src/log/store.rs:78`
- TUI render entry points: `src/tui/render.rs` — `render_preview` (line ~125 message handling), `render_raw` (line ~262)
- Viewport pin-by-seq logic: `src/tui/viewport.rs:97` (`resolve_seq`)
