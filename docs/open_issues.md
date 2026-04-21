# Open Issues

## ~~exec'd processes should appear in the sidebar~~ (RESOLVED)

**Resolved:** `ctx.exec()` is now sugar for `ctx.spawn(cmd).complete().await`. Every exec'd process goes through the `SpawnBuilder`, emits a `SpawnEvent`, and appears in the TUI sidebar while running. Commit `f7660db`.

## Crate naming — `runme` is taken on crates.io

Constraints: short (CLI used frequently), obvious purpose, not taken on crates.io. Project name doesn't have to match the CLI binary name.

### Available candidates

| Crate name | CLI name | Angle | Notes |
|------------|----------|-------|-------|
| `rnme` | `rnme` | compressed "runme" | 4 chars, preserves brand |
| `taskr` | `taskr` | "task runner" | 5 chars, immediately obvious |
| `helm` | `helm` | nautical — steering the ship | 4 chars, but collides with k8s Helm |
| `yawl` | `yw` | nautical — two-masted sailboat | great CLI shorthand |
| `boatswain` | `bo`/`bosn` | nautical — the officer who runs the deck crew | strongest thematic fit, long crate name |
| `mast` | `mast` | nautical — everything hangs from it | 4 chars |
| `spar` | `spar` | nautical — structural beam | 4 chars |
| `yaw` | `yaw` | nautical — rotation/movement | 3 chars |
| `furl` | `furl` | nautical — rolling up sails | 4 chars |

### Taken but possibly reclaimable

| Name | Status | Notes |
|------|--------|-------|
| `tak` | last updated 2016, ~19k downloads | board game impl, dormant 10 years |
| `doit` | last updated 2022, ~1.6k downloads | terminal task manager |

### Explored and taken

Most 2-letter names, `rn`, `mk`, `rr`, `xx`, `tsk`, `werk`, `doit`, `rune`, `forge`, `anvil`, `knot`, `hoist`, `haul`, and many others.

## Carriage return (`\r`) progress output corrupts log display

Commands that use `\r` to update a progress line in-place (e.g., `aws s3 cp`, `curl` progress bars) produce garbled output in the TUI log viewer. The record parser splits on `\n` but `\r`-delimited progress chunks don't end with `\n`, resulting in partial line overwrites rendering as separate log entries that stomp on each other.

The source column also shows truncated/corrupted text because the "source" for exec'd output is the command string, which gets clipped.

Possible approaches:
- Recognize `\r` as an in-place update and replace the previous entry from that source
- Collapse `\r`-delimited chunks into a single entry that updates in place
- Strip `\r` progress output entirely and only show the final line

