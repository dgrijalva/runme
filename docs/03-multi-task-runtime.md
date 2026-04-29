# Multi-Task Runtime

## Goal

Turn runme from a single-task launcher into a multi-task runtime: a task graph in which multiple tasks can run concurrently, be spawned and terminated dynamically, and be observed (status, logs) by external interfaces. The TUI is the primary interface today, but the runtime is designed so other frontends — MCP server, headless CLI, future tools — consume the same engine.

## Status: designing

This document is the working design. Open questions live inline; they get resolved as we build and use the thing.

> **Implementation in progress.** Architecture decisions made during the implementation pass live in `docs/plans/notes/architecture.md` (current source of truth for type definitions, cancellation model, builder shapes, etc.). This document will be updated comprehensively once implementation settles.

## Motivation

`runme` already has the rough shape of multi-task plumbing — `TaskRunner.sessions`, shared `LogStore`, per-execution shutdown — but it lives in the TUI layer (`src/tui/runner.rs`) and the rest of the system assumes a single top-level task. To support multi-task properly (and to not paint ourselves into a corner around future MCP/headless modes), the multi-task model needs to live in the **engine**, with frontends as consumers.

The shift unlocks:

- **TUI:** launch tasks at any time, watch them in parallel, terminate them individually, scroll back through completed ones.
- **MCP:** tools like `spawn_task`, `kill_task`, `query_logs`, `inspect_graph` mapped onto the same engine API.
- **Headless:** `runme <task>` becomes "spawn one child of the root and exit when it ends" — a degenerate consumer of the same runtime.

## Architecture

The runtime has two layers:

1. **Engine** (`src/execution/`): the multi-task task graph, control protocol, exposed state. Frontend-agnostic.
2. **Frontends** (`src/tui/`, future `src/mcp/`, the headless `src/bin/rnme/`): consumers that translate user input into engine control messages and read engine state.

### Design philosophy

The engine/frontend separation **isn't about supporting "any UI."** The frontends are a known, finite set, all in this crate, all known at compile time:

- **CLI / headless** — trigger one task, signal propagation, exit when done.
- **MCP** — trigger tasks, query state, support compound operations like "start this with a timeout, return output filtered by X."
- **TUI** — high-framerate observer of everything; the most demanding consumer. Likely an immediate-mode model: watch the graph (via `tokio::watch` or similar), rebuild the rendered view on change.
- **Direct library API** — theoretical future use case (someone embedding the engine in their own program). Worth not painting ourselves into a corner over, but no specific design pass until it actually comes up.

The point of separating engine from UI **is to avoid baking reusable engine capabilities into a single UI layer** — not to design infinite polymorphism upfront. We won't build generic abstractions for "any observation surface"; we'll expose engine state straightforwardly, and each UI builds whatever consumption pattern fits its needs. New UI-specific surfaces get added when a UI needs them, not preemptively.

```
        ┌──────────────────────────┐  ┌──────────────────────┐  ┌──────────────────┐
        │ TUI (sidebar + picker +  │  │ MCP server (tool     │  │ Headless         │
        │ kill menu + log viewer)  │  │ calls)               │  │ (`runme <task>`) │
        └────────────┬─────────────┘  └──────────┬───────────┘  └────────┬─────────┘
                     │ control msgs              │ control msgs          │ control msgs
                     │ observation               │ observation           │ observation
                     ▼                           ▼                       ▼
        ┌──────────────────────────────────────────────────────────────────────────┐
        │                          Engine                                          │
        │  • Synthetic root task                                                   │
        │  • Recursive task graph (task → tasks/processes)                         │
        │  • Control channel (SpawnTask, KillTask, Quit, ...)                      │
        │  • Exposed state (task graph, log store) — UIs read directly             │
        └──────────────────────────────────────────────────────────────────────────┘
```

## Engine

### Model: tasks are recursive, with a synthetic root

This is the load-bearing model change.

- **Tasks are recursive.** A task can spawn child tasks *and* child processes. Processes are leaves — they don't spawn tasks. The task/process boundary is structural: tasks are in-process Rust code controlled via channels and async; processes are OS processes controlled via signals and syscalls.
- **A synthetic root task sits above all user tasks.** The root is a real `TaskDef`, library-provided, not registered in the user-visible catalog. Its body is essentially "wait for a shutdown signal, return," with a select loop over the engine's control channel mixed in. It exists to anchor the task tree, run the engine's control loop, and provide a place to attach app-lifecycle code.
- **Multi-task means the root spawned N child tasks.** The runtime holds exactly one task (the root). "Two tasks running" = "the root has two child tasks."

### Why this shape

- **Reuse over reimplementation.** All the machinery for "a node with children" — status aggregation, shutdown propagation, log composition — already exists at the per-task level. Inserting the synthetic root means it applies to the multi-task case for free, instead of being reimplemented at a `Vec<TaskSession>` level on the runner.
- **A real handle for runtime-level interaction.** The root task's body and `TaskContext` become the natural attachment point for things that today have nowhere clean to live: control protocol handling, runme-level tracing, cross-task coordination, future things like global shutdown handlers.
- **Composition for RUNME.rs authors.** Once tasks are first-class spawnable units (not just at the runner), users can build tasks that orchestrate other tasks — a real composition primitive.
- **Frontend-agnostic.** The same engine drives TUI, MCP, and headless. None of them need their own task-tracking machinery.

### Control protocol

Frontends interact with the engine by sending typed messages on a channel. The root task awaits messages in a select loop and dispatches.

Approximate shape (bikeshed in implementation):

```rust
pub enum Control {
    SpawnTask { def: &'static TaskDef, args: Vec<String> },
    KillTask  { id: TaskId, signal: KillSignal },  // KillSignal::Term, ::Kill, ::All
    Quit,
}
```

The frontend holds a `Sender<Control>`. The engine constructs this when the runtime starts and exposes the sender as part of its public API.

Receiver-dropped is by definition the shutdown path — the root task exits, the channel closes, the frontend is on its way out.

### Exposed state

The engine exposes its state directly — no abstracted observation API designed up-front. Frontends read what they need, however suits them:

- **Task graph.** The running tree (root → children → grandchildren / processes), exposed as Rust types. Likely shape: `Arc<RwLock<Graph>>` or `tokio::watch` for cheap change-detection. The TUI's preferred consumption is probably "watch the graph, rebuild the rendered view on change" — immediate-mode style.
- **Log store.** `LogStore::subscribe()` (broadcast) already exists as a primitive — it moves to the engine and stays as-is. Not a status surface; just the log stream.

That's the surface. We'll add UI-specific affordances when a UI actually needs one we haven't built. For example, MCP wanting "filter completed task output by regex" is an MCP feature that may live closest to MCP's tool implementation, not a generic "filtered observation" engine API.

The minimum that has to be true at the engine level: the data is there, accessibly typed, and changes are detectable cheaply enough for a 60fps TUI redraw to feel free.

### `TaskContext::run()` becomes graph-aware

The composition primitive already exists: `ctx.run(name, args)` resolves through the registry, creates a fresh `TaskContext` carrying the same registry, and runs the task body. `tasks()` provides the query side.

What changes: every `run()` invocation **materializes as a node in the engine's task graph**. Today `Registry::run_with_registry` just calls `task.func.call(&ctx, args).await` directly — no `TaskExecution` wrapper, no observable "child task started" event. For the multi-task runtime, each call creates a `TaskExecution`-shaped child of the parent's task, with its own status, log source, child-process list, and graph identity.

`run()` returns a `TaskHandle` (mirrors `SpawnBuilder`/`ProcessHandle` for processes — same pattern). The handle is the lifetime token:

- `IntoFuture` on the handle: `.await` resolves to `TaskResult`. The common case stays one-line: `ctx.run("foo", &[]).await?`.
- **Drop without awaiting = cancel.** The task receives a cancellation signal and unwinds. Same RAII pattern as `ProcessHandle`.
- **Detachment is explicit and uses async machinery the developer already knows:** `tokio::spawn(async move { ctx.run("worker", &[]).await })` puts the handle inside the spawned tokio task. The original caller can't reach it; the spawned task awaits it; the child lives as long as the spawned task does. The developer is opting into "I don't need the output, set it free."

Why this shape:

- **No new API verbs.** No `spawn_task` / `run_detached` / `.detach()`. Lifecycle is signaled by ownership, the way Rust developers already think about it.
- **Symmetry with process spawning.** `ctx.spawn(cmd)` already works this way; `ctx.run(name)` mirrors it.
- **The graph keeps causality regardless.** Decision 3 (completed tasks stay) means a completed parent with a still-running child isn't pathological — the graph remembers who spawned whom even after the parent is done.
- **Engine-side cancellation (via `Control::KillTask`) uses the same mechanism.** Each `TaskExecution` carries a cancellation token; the handle's Drop signals it; the engine can also signal it directly when a `KillTask` message arrives. The developer-facing and engine-facing paths converge.

Consequences:

- The root task's body, on receiving `Control::SpawnTask`, calls `tokio::spawn(async move { ctx.run(name, args).await })` (or equivalent) — no special-case spawn primitive. Children of the root naturally outlive any individual picker action because the root holds nothing structurally; the spawned tokio tasks own their handles.
- User RUNME.rs authors get composition for free: tasks calling `ctx.run("subtask").await` show up in the sidebar as nested children, observable just like picker-launched tasks.
- "Set and forget" works exactly as the developer expects from async Rust. Holding the handle = task running. Letting it go = task cancelled (or already done).

### What moves where

- **Out of `src/tui/runner.rs`, into the engine:** session/execution management, log store ownership, shutdown propagation, the multi-task graph.
- **Stays in `src/execution/` (already there):** `TaskExecution` (becomes the recursive node type), child-process tracking.
- **Stays in `src/tui/`:** rendering, input handling, the `App` state, anything frontend-specific. The TUI holds engine handles (control sender, graph reader, log subscriber).

`TaskRunner` as a TUI-layer type largely dissolves once its graph/log/lifecycle responsibilities move to the engine. What's left, if anything, is a thin frontend convenience.

## Frontend: TUI

The TUI is the primary frontend for the multi-task runtime. UX decisions below are TUI-specific; the engine doesn't know about them.

### Vision (UX shape)

- Launching `runme` with no args opens the new-task menu (today's picker).
- From any running task, a key opens the new-task menu — picking a task **spawns** it alongside the current one rather than replacing it.
- "Terminate this task" and "Quit runme" are separate, distinct actions.
- When tasks end, the TUI stays put. Completed tasks remain visible in the sidebar; their logs stay accessible. The user opens the new-task menu manually when they want to launch something else, and quits explicitly with `q`.

### Design decisions

#### 1. Picker is a large overlay, not a full-screen mode

Covers most of the screen, sidebar/log pane visible behind it. Re-entrant from `Normal` mode at any time. Designed to be enhanced later with a split layout for an argument-input form when launching a task.

#### 2. Sidebar focus drives log filtering

Top of the sidebar gets an "All tasks" entry — default selection, shows the unfiltered merged log. Navigating to a task filters the log pane to that task and its children (processes).

The full sidebar redesign is deferred — needs to be tried in real use before locking down — but the filtering rule is settled. Note: the sidebar's "All tasks" entry is a UI-layer affordance; in the engine model it corresponds to focusing the synthetic root.

#### 3. Completed tasks stay around

Their logs remain in the engine's log store, their sidebar entries remain focusable. Memory pressure is a future problem worth tolerating for the navigability win.

**Presentation TBD** — try a few:
- Inline with the running tasks, marked as "Done"
- A separate "Completed" section below the running tasks
- Inline by default, with a show/hide toggle for completed entries

#### 4. Kill submenu under `k`

Mirrors the `c` (copy) submenu pattern. Initial bindings:

| Key | Action |
|-----|--------|
| `k` | Normal terminate the focused task (so `kk` is the natural "kill this") |
| `9` | SIGKILL the focused task |
| `a` | Normal terminate **all** tasks |

Each binding maps to a `Control::KillTask` variant. More to come; revisited in the keybinding redesign pass.

#### 5. Duplicate-source disambiguation: color first, numbering as fallback

When two tasks share a source string (e.g. two `cargo build`), distinguish them visually via the existing source-color system. Only fall back to numbered prefixes (`[2] cargo build`) when colors run out. Part of the sidebar redesign work.

#### 6. Ship multi-task before the keybinding redesign

Pre-release software, single user. Don't engineer around future churn. Multi-task plumbing first, then a coherent keybinding pass on top of the new shape.

#### 7. TUI lifecycle: stays open until explicit quit

Once the TUI is up, it stays up regardless of task state. Last task ending does **not** auto-open the picker and does **not** close the TUI. The user looks at logs, restarts processes, opens the picker manually, or presses `q` to quit.

This lets us **drop `tui_wait` and `tui_output` entirely**. There's only one TUI lifecycle behavior; no per-task or per-runner gating needed.

`q` sends `Control::Quit`. If any tasks are still running, prompt to confirm before sending.

#### 8. Picker is always an overlay

Even on first launch with no tasks running, the picker is an overlay over an empty TUI shell (sidebar with just "All tasks", empty log pane). One visual model for the picker, no special "first launch" mode.

#### 9. Picker → engine via control channel

Picker selection emits `Control::SpawnTask { def, args }` to the engine's control channel. The root task receives, calls `ctx.spawn_task(...)`, and the new child appears in the graph (and via observation in the TUI).

This leans into the core idea of the design: the root is a real task using existing task-spawning infrastructure, not a special-cased orchestrator. The picker's wiring is the same shape MCP's `spawn_task` tool call would use.

## Frontend: MCP (future)

Stub — placeholder for design once we get there. The shape is clear: tool calls (`spawn_task`, `kill_task`, `query_logs`, `inspect_graph`, `quit`) translate into `Control` messages or observation reads. MCP-specific concerns (tool schemas, authentication, streaming responses for long-running queries) belong in their own pass.

## Frontend: Headless (`runme <task>`)

Stub — `runme <task>` becomes a degenerate consumer: start the runtime, send `Control::SpawnTask` for the requested task, await its completion, then `Control::Quit`. No TUI. Currently this path doesn't even use a runtime as such — refactoring to go through the engine unifies the code paths.

## Open design questions

Things we'll need positions on but can defer to implementation or first encounter:

- **Sidebar redesign scope.** Multi-task tree, completed section, source colors. Deferred until we can use a working multi-task implementation, but will need its own pass.
- **Source identity model.** Whether to namespace sources by task ID at the log store level (clean, slightly invasive), or only at the display layer (cheap, leaves potential for filter ambiguity). Decide once we hit a real collision.
- **Picker → argument form.** Eventual split layout for tasks that take arguments. Out of scope for the first pass but the picker overlay should be designed to accommodate it.

## Future considerations

Ideas raised during design that are out of scope for this pass but worth keeping on file:

### Per-task TUI quit preference

Tasks could declare a preferred behavior for "what happens to the TUI when I finish":

- `Quit` — close the TUI when done (good for one-shot tasks like `clean`)
- `Stay` — keep the TUI open (good for long-running watchable tasks)
- `NoOpinion` — defer to context

In a multi-task context, preferences are resolved across all running tasks: `Stay` wins if any task wants it; `Quit` only wins if every task wants to quit (or has no opinion). Default with no opinion is `Stay`.

Likely subsumed in practice by the existing CLI-vs-TUI default mode (`ui_hint` on `TaskDef`): a task that's just "run and throw away" probably defaults to CLI mode and never opens the TUI in the first place. So this feature may not need to ship at all. Revisit once the multi-task TUI is in real use.

## Implementation order

Two phases. Phase 1 lands the engine; Phase 2 rewires the TUI onto it. MCP and headless slot in afterwards as separate efforts.

Each step should land as a working slice. Steps within a phase are listed in rough dependency order.

### Phase 1: Engine

1. **Synthetic root task.** Define the library-provided root `TaskDef`. Its body waits on a shutdown signal and returns. Build it as a real task using existing task infrastructure.
2. **Make `ctx.run()` graph-aware.** Each invocation creates a `TaskExecution`-shaped child node in the engine's task graph (status, log source, child processes, graph identity). Today `run_with_registry` just calls the task function directly; this becomes the new path. Public API stays the same.
3. **Recursive task graph.** Make the existing per-task tracking (status, processes, log store) compose recursively so the synthetic root's children are first-class observable nodes. Move log store ownership to the engine.
4. **Control protocol.** Define `Control` message type and wire the root's body into a select loop on the control channel. `SpawnTask`, `KillTask`, `Quit`.
5. **Observation API.** Public engine handles for graph reads, log subscription, and status events.
6. **Engine entry point.** A clean `Engine::start() -> EngineHandle` (or similar) that frontends use. Headless and TUI both call it.

### Phase 2: TUI

7. **Drop the singletons and `tui_wait`.** Remove `task_status`, `task_name`, `processes`, `tui_wait`, `tui_output` from `AppState` and `TaskRunner`. State now reads through the engine's observation API. Drop `tui_wait`/`tui_output` plumbing from `LaunchConfig` and `TaskExecution`.
8. **TUI rewires onto the engine.** `App` holds an `EngineHandle`; rendering reads graph and logs from it; input emits `Control` messages.
9. **Picker as overlay, always re-entrant.** Make `AppMode::TaskPicker` an overlay rather than a full-screen mode-switch; bind a key (likely `n`) to open it from `Normal`. Picker overlays an empty shell on first launch. Selection emits `Control::SpawnTask`.
10. **Sidebar restructure.** "All tasks" entry at top. Child-task entries showing nested processes. Sidebar focus drives the log filter (read from observation).
11. **Kill submenu** (`k`) wired to `Control::KillTask`.
12. **Quit semantics.** `q` emits `Control::Quit` (with confirmation prompt if tasks running). `k a` emits `KillTask::All`.
13. **Source disambiguation.** Color-first, numbered fallback.

### Phase 3 and beyond (out of scope here)

- Headless mode rewires through the engine (`runme <task>` → `SpawnTask` → await → `Quit`).
- MCP server frontend.

## Risks and concerns

- **Resource use grows linearly with task count.** No backstop today against spawning 50 long-running tasks. Probably fine for a single user; revisit if it bites.
- **Readiness conditions** (`ready_on_port`, etc.) are per-process. Multi-task should "just work" but verify two tasks watching different ports don't interfere.
- **Sidebar gets busier fast.** Two tasks each spawning 3 processes is already 8 entries before completed tasks. The redesign needs to handle density (collapsing, grouping, filtering) without becoming a separate project.
- **Engine API stability.** Once MCP starts depending on the engine's public API, breaking changes get more expensive. Pre-release status mitigates this for now, but the engine API is the surface we'll most regret churning later. Worth being a bit more deliberate there than in the TUI.
