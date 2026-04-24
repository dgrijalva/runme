# Open Issues

## ~~exec'd processes should appear in the sidebar~~ (RESOLVED)

**Resolved:** `ctx.exec()` is now sugar for `ctx.spawn(cmd).complete().await`. Every exec'd process goes through the `SpawnBuilder`, emits a `SpawnEvent`, and appears in the TUI sidebar while running. Commit `f7660db`.

## ~~Crate naming — `runme` is taken on crates.io~~ (RESOLVED)

**Resolved:** Renamed to `rnme`. Library and CLI binary are a single merged crate — `cargo install rnme` gives you the binary, `use rnme::prelude::*` gives you the library. RUNME.rs filename convention kept as-is for readability.

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:
- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line

