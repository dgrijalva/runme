# Open Issues

## exec'd processes should appear in the sidebar

`ctx.exec()` runs a command and waits inline, but it doesn't emit a `SpawnEvent`, so the process never appears in the TUI sidebar. Only `ctx.spawn()` creates sidebar entries. For long-running exec'd operations (e.g., `aws s3 cp`, `curl` uploads), the user has no visibility into what's running or how many parallel operations are active.

`exec` should emit a `SpawnEvent` (or equivalent) so the process appears in the sidebar while it's running, and is removed or marked done when it completes. This is the whole reason task code calls `ctx.exec()` instead of running commands directly — the runtime should have full visibility.

The parallel `try_join_all` of exec'd commands is a good test case: the sidebar should show N concurrent downloads, each with their own status.

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:
- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line

