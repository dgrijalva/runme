# Multi-Task Runtime Implementation

## Goal

Implement the multi-task runtime design from `docs/03-multi-task-runtime.md`. End state:

- Engine layer (`src/execution/`) supports a recursive task graph with a synthetic root task.
- `TaskContext::run()` becomes graph-aware (each invocation materializes as a node, returns a `TaskHandle` with drop-cancels semantics).
- Frontends (TUI today, MCP/headless future) consume the engine via a control channel + exposed state.
- TUI is rewired onto the engine and supports running multiple tasks concurrently with a kill submenu, persistent picker, and completed-task retention.

## Approach

Two phases mirroring the design doc, plus an upfront research/architecture pass to nail down internal types before code is written.

1. **Research + Architecture** — Three parallel researchers ground the team in existing code; an architect synthesizes their findings into concrete internal type definitions. Human review gate before implementation begins.
2. **Phase 1: Engine** — Four sequential slices, each with implementor + validator. Coupled work; can't parallelize without conflicts on shared types.
3. **Phase 2: TUI** — One rewire slice (highest risk), then sequential UX slices. File contention on `src/tui/app.rs` and `src/tui/keys.rs` makes this mostly sequential too.

Strategies per phase:

- Research: **Subagent Pool** (independent questions, no coordination needed)
- Architecture: **single agent** (synthesis)
- Engine + TUI: **Agent Team** (coupled implementation, coordination via plan approval and inter-agent messages)

## Acceptance Criteria

- [ ] Multiple tasks can run concurrently in the TUI; each appears as its own entry in the sidebar
- [ ] Tasks spawned via the picker show in the sidebar with their own log source
- [ ] `ctx.run("subtask")` from inside a user task creates an observable graph node (sidebar entry)
- [ ] `TaskHandle` cancels its task when dropped without being awaited
- [ ] `tokio::spawn(async move { ctx.run(...).await })` correctly detaches a child to live independent of the parent's await
- [ ] Tasks can be killed individually via the kill submenu (`kk` for SIGTERM, `k9` for SIGKILL, `ka` for terminate-all)
- [ ] `q` quits the TUI; if any tasks are running, prompts to confirm before tearing down
- [ ] Completed tasks remain visible in the sidebar with their logs accessible
- [ ] Picker is a re-entrant overlay opened with `n`; can be opened any time tasks are running or not
- [ ] All existing tests pass (`cargo test`)
- [ ] Headless `runme <task>` mode still works (regression)
- [ ] Single-task TUI invocation (`runme <task>`) still works during and after Phase 1 (regression)

## Human Review Gates

| Gate | After | Classification | Rationale |
|---|---|---|---|
| Architecture review | Architecture phase | **Requires review** | Internal type shapes commit us to an API; hard to reverse mid-implementation |
| Engine complete | Phase 1 slice 4 | **Requires review** | Engine surface is what MCP will eventually depend on; worth checking before locking in |
| Multi-task UX | After Phase 2 | **Requires review** | UX needs hands-on validation, not just tests |
| Per-slice validation | Each slice | **Auto-approve** if tests pass | Internal refactors with passing tests proceed |

## Status

draft

## Context

### Source design

`docs/03-multi-task-runtime.md` — full design including engine architecture, control protocol, exposed state, TUI decisions (9 settled), and implementation order.

### Key existing code

- `src/execution.rs` — `TaskExecution`, `TaskStatus`, `ProcessInfo`, `LaunchConfig`. Single-task today.
- `src/task.rs` — `TaskContext`, `Registry`, `TaskDef`, `run()`, `tasks()`, spawn/exec primitives.
- `src/tui/runner.rs` — `TaskRunner` (multi-session scaffolding that mostly dissolves).
- `src/tui/app.rs` — `AppState` with singleton fields (`task_status`, `task_name`, `processes`, `tui_wait`, `tui_output`) that go away.
- `src/builtin.rs` — Where the synthetic root `TaskDef` will live (built-in but not registered in catalog).

### Constraints

- Pre-release software, single user — no backwards compatibility concerns. APIs can change freely.
- `tui_wait` and `tui_output` plumbing is being removed entirely (settled in design decision 7).
- Rust edition 2024.

## Team

| Name | Role | Type | Model | Strategy | Plan Approval |
|---|---|---|---|---|---|
| researcher-execution | Investigate current `TaskExecution` lifecycle, spawn events, log routing | Explore | Opus | subagent | n/a |
| researcher-cancellation | Pick the right cancellation primitive (tokio_util `CancellationToken` vs alternatives); design integration | Explore | Opus | subagent | n/a |
| researcher-task-context | Map current `TaskContext::run` / `Registry::run_with_registry` flow; identify where graph identity gets injected | Explore | Opus | subagent | n/a |
| engine-architect | Synthesize research; produce internal type definitions, parent-child wiring spec, `TaskHandle` shape | Plan | Opus | team | yes |
| engine-impl-1 | Implement Slice 1: `Control` enum, root `TaskDef`, root body select loop | general-purpose | Opus | team | yes |
| engine-validator-1 | Build, test, regression-check after Slice 1 | Bash | Sonnet | team | no |
| engine-impl-2 | Implement Slice 2: Recursive `TaskExecution`, parent-child wiring, `TaskId` assignment, log store ownership | general-purpose | Opus | team | yes |
| engine-validator-2 | Build, test, regression-check after Slice 2 | Bash | Sonnet | team | no |
| engine-impl-3 | Implement Slice 3: `ctx.run()` becomes graph-aware, `TaskHandle` with drop-cancels | general-purpose | Opus | team | yes |
| engine-validator-3 | Build, test, regression-check after Slice 3 | Bash | Sonnet | team | no |
| engine-impl-4 | Implement Slice 4: `Engine::start()` entry point, `KillTask` wired to cancellation, headless rewires through engine | general-purpose | Opus | team | yes |
| engine-validator-4 | Build, test, full regression suite after engine complete | Bash | Sonnet | team | no |
| tui-rewire | Drop singletons + `tui_wait`/`tui_output`, rewire `AppState` onto engine handles | general-purpose | Opus | team | yes |
| tui-rewire-validator | Verify single-task TUI still works after rewire (no UX change yet) | Bash | Sonnet | team | no |
| tui-impl-picker | Picker as re-entrant overlay; `n` opens it; selection emits `Control::SpawnTask` | general-purpose | Opus | team | yes |
| tui-impl-sidebar | Sidebar with "All tasks" entry, child tasks, processes; focus drives log filter | general-purpose | Opus | team | yes |
| tui-impl-killmenu | Kill submenu under `k` (`kk`, `k9`, `ka`) wired to `Control::KillTask` | general-purpose | Opus | team | yes |
| tui-impl-quit | Quit semantics: `q` emits `Control::Quit` with running-task confirmation | general-purpose | Sonnet | team | no |
| tui-impl-sources | Source disambiguation: color first, numbered fallback | general-purpose | Sonnet | team | no |
| tui-validator | Build, test, manual UX smoke after each TUI slice | Bash | Sonnet | team | no |

## Phases

### Phase R: Research (parallel)

| Task ID | Assigned To | Depends On | Parallel | Description |
|---|---|---|---|---|
| research-execution | researcher-execution | none | yes | Read `src/execution.rs` end-to-end. Document: how `TaskExecution::launch` wires the task body; how `monitor_spawns` routes spawn events; how `LaunchConfig` is used today; what holds JoinHandles; how `shutdown` propagates. Output: `docs/plans/notes/research-execution.md` |
| research-cancellation | researcher-cancellation | none | yes | Survey cancellation options. Decide: `tokio_util::sync::CancellationToken` vs `JoinHandle::abort` vs custom signaling. Consider: does cancellation propagate naturally through `.await` points in user task bodies? How do parent-child cancellation chains compose? Output: `docs/plans/notes/research-cancellation.md` with a recommended primitive and rationale. |
| research-task-context | researcher-task-context | none | yes | Read `src/task.rs` (`TaskContext`, `Registry`, `run`, `run_with_registry`). Document the call graph from `ctx.run("foo")` down to the task function executing. Identify the seams where graph-aware `run()` would add a `TaskExecution` wrapper. Output: `docs/plans/notes/research-task-context.md`. |

### Phase A: Architecture

| Task ID | Assigned To | Depends On | Parallel | Description | Human Review |
|---|---|---|---|---|---|
| architecture | engine-architect | research-execution, research-cancellation, research-task-context | no | Synthesize findings. Define: the recursive `TaskExecution` shape (parent reference, children list, `TaskId`); `TaskHandle` API (IntoFuture, Drop); the `Control` enum; the `Engine` type and its public surface (control sender, graph reader, log subscribe); how the synthetic root task's body is structured; cancellation token integration. Update `docs/03-multi-task-runtime.md` with concrete type signatures, OR write `docs/plans/notes/architecture.md`. Identify any decisions the design doc didn't cover. | **Yes — required** |

### Phase 1: Engine

| Task ID | Assigned To | Depends On | Parallel | Description | Human Review |
|---|---|---|---|---|---|
| engine-slice-1 | engine-impl-1 | architecture | no | Slice 1: Control + root scaffold. Define `Control` enum (`SpawnTask`, `KillTask`, `Quit`). Define synthetic root `TaskDef` in `src/builtin.rs` (not registered via inventory). Root body: select loop on `Receiver<Control>`. `Quit` exits. `SpawnTask` calls `ctx.run(name, args).await` for now (graph-tracking comes in Slice 3). `KillTask` stub. Add a unit test demonstrating root receives `Quit` and exits. | Auto-approve |
| validate-slice-1 | engine-validator-1 | engine-slice-1 | no | `cargo build && cargo test`; verify single-task TUI still launches (`runme :list` smoke). | Auto-approve |
| engine-slice-2 | engine-impl-2 | validate-slice-1 | no | Slice 2: Recursive `TaskExecution` + `TaskId`. Add monotonic `TaskId`. Add parent reference + children list to `TaskExecution`. Move `LogStore` ownership from `TaskRunner` to a new top-level `Engine` (or extend `TaskExecution` parenting). No behavior change yet — single-task path still works. | Auto-approve |
| validate-slice-2 | engine-validator-2 | engine-slice-2 | no | Build, test, single-task regression. | Auto-approve |
| engine-slice-3 | engine-impl-3 | validate-slice-2 | no | Slice 3: `ctx.run()` graph-aware + `TaskHandle`. Each `ctx.run(name, args)` returns a `TaskHandle`. `IntoFuture` resolves to `TaskResult`. Drop without awaiting cancels via the cancellation token (per architecture). `run_with_registry` now creates a child `TaskExecution` parented to the caller's. Add tests: handle awaited returns Ok; handle dropped cancels; child appears in parent's children list. | Auto-approve |
| validate-slice-3 | engine-validator-3 | engine-slice-3 | no | Build, test, single-task regression. | Auto-approve |
| engine-slice-4 | engine-impl-4 | validate-slice-3 | no | Slice 4: `Engine::start()` + `KillTask` + headless rewire. Define `Engine` public type with `start()` returning a handle (control sender + state reader + log subscribe). Wire `KillTask` to look up the task by ID and cancel its token. Refactor headless `runme <task>` to start `Engine`, send `SpawnTask`, await completion, send `Quit`. Engine-complete review gate. | **Yes — required (engine API surface)** |
| validate-slice-4 | engine-validator-4 | engine-slice-4 | no | Full test suite + headless regression + single-task TUI regression. | Auto-approve |

### Phase 2: TUI

| Task ID | Assigned To | Depends On | Parallel | Description | Human Review |
|---|---|---|---|---|---|
| tui-rewire | tui-rewire | validate-slice-4 | no | Drop `task_status`, `task_name`, `processes`, `tui_wait`, `tui_output` from `AppState` and `TaskRunner`. Drop `tui_wait`/`tui_output` from `LaunchConfig` and `TaskExecution`. `AppState` holds an `EngineHandle` (or equivalent); rendering reads graph + logs from it. **No UX change yet** — single-task TUI should look identical to a user. | Auto-approve |
| validate-tui-rewire | tui-rewire-validator | tui-rewire | no | Verify single-task TUI launches and renders identically to before. Run `runme :list`, `runme builtin:check` (or any existing tasks). | Auto-approve |
| tui-picker | tui-impl-picker | validate-tui-rewire | no | Make `AppMode::TaskPicker` an overlay; bind `n` to open from `Normal`. Selection emits `Control::SpawnTask`. Picker overlays empty shell on first launch. | Auto-approve |
| tui-sidebar | tui-impl-sidebar | tui-picker | no | "All tasks" entry at top (focuses the synthetic root via the model). Each child task renders with its nested processes. Sidebar focus drives the log filter (read from observation API). | Auto-approve |
| tui-killmenu | tui-impl-killmenu | tui-sidebar | no | Kill submenu under `k`. `kk` → `KillTask::Term { id }`, `k9` → `KillTask::Kill { id }`, `ka` → `KillTask::All`. | Auto-approve |
| tui-quit | tui-impl-quit | tui-killmenu | no | `q` emits `Control::Quit`. If any tasks running, prompt to confirm before sending. | Auto-approve |
| tui-sources | tui-impl-sources | tui-quit | no | Duplicate-source disambiguation. Use existing source-color system; fall back to `[N]` numbered prefixes when colors run out. | Auto-approve |
| validate-tui | tui-validator | tui-sources | no | Full test suite + manual UX smoke covering all acceptance criteria. | **Yes — required (UX validation)** |

## Validation Profile

```yaml
validation:
  build:
    command: "cargo build"
    required: true
  tests:
    command: "cargo test"
    required: true
  fmt:
    command: "cargo fmt --check"
    required: false
  clippy:
    command: "cargo clippy -- -D warnings"
    required: false
  regression-headless:
    command: "cargo run -- :list"
    required: true
    description: "Headless mode prints task list and exits cleanly"
```

## Findings

(populated during research)

## Decisions Log

(populated during execution; significant decisions captured here)

## Blockers

(populated during execution if anything blocks)

## Notes

- The design doc (`docs/03-multi-task-runtime.md`) is authoritative for all architecture decisions. If anything in this plan conflicts, the design doc wins.
- Each engine slice should be independently buildable and testable. If a slice can't pass tests without the next one, the boundary was drawn wrong — re-slice.
- Engine slices are sequentially dependent. TUI slices have file contention (`app.rs`, `keys.rs`, `render.rs`) so are mostly sequential too.
- The "open design questions" remaining in the design doc (sidebar redesign scope, source identity model, picker → argument form) are deferred to first-encounter or future passes — don't try to resolve them in this plan.
