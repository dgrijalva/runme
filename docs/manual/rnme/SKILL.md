---
name: rnme
description: Use this skill when you have access to the rnme MCP server and need to run, monitor, or manage tasks in a project. Covers list_tasks, spawn_task, run_task, get_task, get_logs, grep_logs, kill_task, and the rest of the rnme MCP tool surface — what each tool does, the address format for task IDs, how to read task reports, how to subscribe to logs, and which tool to reach for in common operating workflows.
---

# Operating rnme

`rnme` is the project's task runner. When you see the `mcp__runme__*` tools, the project has a running `rnme --mcp` supervisor and tasks are defined in `RUNME.rs` files in the project tree. This skill teaches you how to drive it.

For authoring or extending RUNME.rs files, see the `rnme-author` skill.

## Tool inventory

The supervisor exposes these tools (names below are the MCP tool name, prefixed with `mcp__runme__` in your namespace):

| Tool | Shape | What it does |
|---|---|---|
| `list_tasks` | read | Enumerate available tasks. Always start here. |
| `spawn_task` | spawn | Start a task in the background. Returns immediately with `{task_id, initial_seq}`. |
| `run_task` | spawn + wait | Spawn and wait for terminal status. Returns the rendered task report. |
| `get_task` | read | Fetch the rendered task report (running or completed). |
| `get_logs` | read | Cursor-paged log entries, with optional filter. |
| `grep_logs` | read | Regex over log message/raw text. |
| `get_graph` | read | Merged task/process tree across all live engine generations. |
| `get_build_status` | read | Compilation status of the user's RUNME.rs files. |
| `kill_task` | mutate | Cancel a top-level task and its entire subtree. |
| `kill_process` | mutate | Signal a single process without cancelling its parent task. |
| `kill_all` | mutate | Cancel every direct child of root — your "stop everything" button. |
| `install_skills` | meta | Skills bootstrap. Ignore unless explicitly asked. |

## Always begin here

Call `list_tasks` first. The result is `{ "tasks": [{ name, group, qualified_name, description, args_help }, ...] }`.

- `qualified_name` is the form to pass to `spawn_task` / `run_task`. Root-group tasks have `qualified_name == name`. Tasks in a sub-group look like `services/api:start`.
- `args_help` is present on tasks that accept arguments. If absent, the task takes no args.
- `description` comes from the task's doc comment. Read it before invoking.

If `list_tasks` returns an error indicating no `RUNME.rs` was found: tell the user, don't loop. They may need to run `rnme --init` to seed one.

## The two execution shapes

### `run_task(name, args?, timeout_seconds?, tail_n?)` — blocking

Spawns and waits until the task reaches a terminal status (`Done` / `Failed` / `Cancelled` / `Timeout`). Returns the rendered task report (a multi-line string).

Use for: builds, tests, deploys, anything one-shot. This is "do the thing and tell me how it went" in a single call. No polling on your side.

### `spawn_task(name, args?, timeout_seconds?)` — fire-and-forget

Returns `{ task_id, initial_seq }` immediately. The task keeps running.

Use for: long-running services (dev servers, watchers, file-watchers) you want to keep alive while you exercise them with other tools.

`initial_seq` is the log sequence at the moment of spawn — pass it as `since_seq` to `get_logs` to subscribe-from-spawn without missing the first entries.

## Addresses

Task IDs are dotted strings like `t42` or `t42.7`.

- The `t` prefix is for display. **When passing IDs back as arguments, drop the `t`** — the input format is `42` or `42.7`.
- First segment: the top-level task (the user-spawned ancestor). Routes the call to the right engine.
- Second segment: a sub-task or process inside that subtree.

`t42` refers to the whole top-level task; `t42.7` refers to one specific process inside it. Use the bare top-task address with `kill_task` to cancel everything; use the dotted process address with `kill_process` if you only want to signal one process.

## Reading what a task did

### `get_task(id, tail_n?)` — the primary "how did it go" tool

Returns the rendered task report. Shape:

```
Task t42 build - completed (exit 0)
Started: 2026-05-08 10:14:23  Run time: 4.2s
Stdout: 87 lines, CargoDiag 73%
Stderr: 12 lines
Events: 3 lines
Summary:
Build succeeded. 0 warnings. Output: target/debug/myapp.
```

Fields:
- **Status**: `completed (exit 0)` / `failed: <reason>` / `cancelled` / `timed out` / `running (setup)` / `running (ready)`. The `failed:` line names the cause directly — read it before guessing.
- **Stdout / Stderr / Events**: aggregated line counts across all descendant processes. Format hints (`JSON 91%`, `CargoDiag 73%`) tell you what kind of output dominates.
- **Events**: `info!()` / `error!()` calls and `ctx.println(...)` from task code. This is where structured task-authored output lives.
- **Summary**: present if the task called `ctx.summary(...)`. When present, it replaces the `Last n lines:` tail — the task is telling you what mattered.
- **Last n lines**: tail of interleaved log output, used as fallback when no `Summary` was set. Defaults to 50 lines; configurable via `tail_n` (capped at 1000).

The report is for reading. Don't try to parse it programmatically — use `get_logs` / `get_graph` for structured data.

### `get_logs(task_id, since_seq?, until_seq?, limit?, filter?)`

Returns `{ entries, next_seq, has_more }`. Cursor-paged.

- `since_seq` is **exclusive**, `until_seq` is **inclusive**. To page forward: pass `since_seq=next_seq` from the previous call until `has_more` is `false`.
- `limit` defaults to 200, caps at 5000. Don't ask for more than you'll read.
- `filter` is an rnme filter expression — `level=error`, `field.k=value`, etc. See [filter-syntax.md](./filter-syntax.md).
- Entries cover the named task **and all its descendants** (subtasks + processes), interleaved by global sequence.

To tail a running task: pass `since_seq=initial_seq` from the spawn return, then keep advancing `since_seq` to `next_seq`.

### `grep_logs(task_id, pattern, limit?, scope?)`

Regex over each entry's message (or raw text if unparsed). Two scopes:
- `descendants` (default): the task and everything under it.
- `self_only`: just entries whose source is exactly `task_id`.

Best for "find me the error in this noisy output" — `grep_logs(id, "ERROR|panic", limit=50)`.

### `get_graph()`

Returns the full merged task/process tree across all live engine generations. Use when you need structured info about who-spawned-what or to enumerate currently running tasks.

## Killing things

### `kill_task(id, signal?)`

Cancels the named top-level task and its **entire subtree**. Always returns `{"ok": true}` on success.

- `signal: "term"` (default): runs the cooperative cancel ladder — gives the task body a chance to clean up, then escalates to SIGKILL on owned processes after a few seconds.
- `signal: "kill"`: immediate SIGKILL, no cleanup grace.

Default to `term`. Use `kill` only when a task is wedged.

### `kill_process(id, signal?)`

Signals a single process inside a task without cancelling the parent task. Use sparingly — usually you want `kill_task`.

### `kill_all()`

Cancels every direct child of root. Your "stop everything I started" button. After it returns, the supervisor stays up; you can list tasks and spawn new ones.

There is no MCP `quit`. The supervisor's lifetime is owned by the agent's MCP session, not by you.

## Build state — when things aren't just-working

`get_build_status()` returns `{ state, last_failure_output?, searched_from? }`. States:

- `idle` — current build is good. Spawns will run immediately.
- `rebuilding` — file edit triggered a recompile. Spawn-shaped calls (`spawn_task`, `run_task`, `list_tasks`) will block briefly until done. Existing-task calls (`get_task`, `get_logs`, `kill_task`) work normally.
- `last_build_failed` — most recent rebuild produced a cargo error. Spawn-shaped calls return that error directly. `last_failure_output` has the full cargo output. Existing live tasks from before the failed rebuild are unaffected.
- `no_task_file` — no `RUNME.rs` discovered. Tell the user.

Key guarantees:
- After a successful rebuild, **previous-generation tasks stay queryable for the entire MCP session**. You don't lose log access just because the user edited code.
- When a build fails, spawns return a cargo-error head. Call `get_build_status` for the full output before reporting back.
- Editing one task does not kill an unrelated running service.

## Common workflows

### 1. Run a build, surface the result

```
run_task("build")
→ read returned report. If status is "failed: ...", the message line names the cause.
```

### 2. Start a service, exercise it, kill it

```
{task_id, initial_seq} = spawn_task("dev-server")
# Wait for the task to enter "ready" status:
get_task(task_id)  # check status; loop with pauses if still in "setup"
# Or look for a known startup line:
grep_logs(task_id, "listening on", limit=1)
# Do whatever testing you need...
kill_task(task_id)
```

Don't tight-loop on `get_task` — pause between checks. A few seconds per check is fine.

### 3. Find the error in noisy output

```
grep_logs(task_id, "ERROR|panic|failed", limit=50)
```

### 4. Page through long output

```
{entries, next_seq, has_more} = get_logs(task_id, limit=200)
# loop until has_more is false:
get_logs(task_id, since_seq=next_seq, limit=200)
```

### 5. Tail a running task

```
{task_id, initial_seq} = spawn_task("watcher")
seq = initial_seq
while still_interested:
    {entries, next_seq, has_more} = get_logs(task_id, since_seq=seq, limit=50)
    # process entries...
    seq = next_seq
    # pause before next pull
```

### 6. Pre-flight a build before spawning

```
{state, last_failure_output} = get_build_status()
if state == "last_build_failed":
    # tell the user about last_failure_output, don't try to spawn
elif state == "rebuilding":
    # spawns will block briefly; that's usually fine
else:
    # idle or no_task_file — handle accordingly
```

## Things to avoid

- **Don't tight-loop `get_logs` or `get_task`.** Pause between calls. A few seconds is fine. A faster cadence wastes tokens and rarely surfaces anything sooner.
- **Don't ask for `tail_n` larger than ~100** unless you're sure you need it. The renderer caps at 1000.
- **Don't parse the rendered report programmatically.** It's prose. Use `get_logs` / `get_graph` for structured data.
- **Don't pass `t0` or guess at addresses.** Top-task IDs only come from `spawn_task` / `run_task` returns or from `get_graph`.
- **Don't assume `kill_all` shuts down the supervisor.** It doesn't. There's no MCP-side quit; the agent's session owns the lifetime.
- **Don't stack mode flags or try to control the supervisor's process.** You only see it through MCP tools.

## See also

- [filter-syntax.md](./filter-syntax.md) — log filter expression grammar for `get_logs`.
