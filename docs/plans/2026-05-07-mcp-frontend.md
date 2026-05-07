# MCP Frontend Implementation

**Status:** approved (G0 cleared; ready for Phase 1)
**Source design:** `docs/mcp_design.md`
**Created:** 2026-05-07
**Approved:** 2026-05-07

> **Design doc deltas (the design doc is stale on these points; treat this plan as authoritative):**
> - **No generation cooldown.** Old gens with completed tasks stay alive for the supervisor's lifetime. The MCP session is the retention boundary, not a 15-minute timer. Drop §"Generations" cooldown logic, the `--gen-cooldown` flag, and the "what survives a generation's retirement: nothing" subsection's cooldown framing — only never-had-tasks gens retire mid-session, and they retire immediately.
> - **`--mcp` runs in the outer driver, not the runner.** `src/bin/rnme/main.rs` short-circuits on `--mcp` *before* `compile_workspace()`. Supervisor lives in the outer process. `current_exe()` returns the outer rnme binary; spawning `current_exe() --engine` re-enters outer rnme which transparently does discover+compile+exec.
> - **`:install_skills` (underscore), not `:install-skills`.** Function names can't have hyphens; `#[rnme::task]` derives the registered name from the function ident. Update design doc references at impl time.

---

## Goal

Implement the MCP (Model Context Protocol) frontend as specified in `docs/mcp_design.md`. End state: a coding agent can connect to `rnme --mcp` over stdio, list tasks defined in the user's RUNME.rs files, run them (foreground or backgrounded), monitor their output, kill them, and install agent skills — and the connection survives RUNME.rs edits, with long-running tasks isolated across generations.

## Approach

Plan is split into **eight phases**, each with its own human review gate. Phases 2–7 land cleanly buildable slices; Phase 8 is end-to-end validation. The design document is the spec — this plan does not relitigate design decisions, only sequences and parallelizes the work.

```
Phase 0  Context research            ─ parallel research, then synthesis gate
Phase 1  Foundational engine changes ─ LogStore seq, TaskExecution timestamps/summary, TaskInfo
Phase 2  Wire protocol + transport   ─ wire.rs types, JSONL framing
Phase 3  Engine daemon (--engine)    ─ TCP server, port-line handshake, subscription registry
Phase 4  Supervisor core (--mcp)     ─ TCP client, address routing, snapshot merge, generations
Phase 5  Build state + file watcher  ─ debounced rebuild, build state machine, generation cooldown
Phase 6  MCP tool surface            ─ rmcp wiring, tool impls, report renderer
Phase 7  Skills bundle               ─ docs/manual/ content, install_skills + :install-skills
Phase 8  Integration tests           ─ end-to-end agent flows, edit-during-task scenarios
```

Phases can overlap where dependencies allow — Phase 2 can start as Phase 1's API surface stabilizes, Phase 7 (skills content) is mostly independent and can land any time after Phase 0.

## Acceptance Criteria

### Functional

- [ ] `rnme --engine` binds a TCP listener on `127.0.0.1:0`, prints `{"port": <u16>}` on stdout, and accepts a single supervisor connection
- [ ] `rnme --engine --start-task-id N` starts the `TaskId` counter at N
- [ ] `rnme --mcp` exposes the MCP tools listed in design §"Tool surface" over stdio via `rmcp`
- [ ] Spawning a task via `spawn_task` returns a dotted `<top>` id; `get_task` / `get_logs` / `kill_task` accept either `<top>` or `<top>.<task>`
- [ ] Editing RUNME.rs while tasks are running spawns a new generation; in-flight tasks keep running on their original generation; new spawns route to the latest build
- [ ] When a build fails, `spawn_task` / `run_task` / `list_tasks` return an error containing the cargo output head; existing-state tools (`get_logs`, `kill_task`, etc.) continue to work
- [ ] `get_build_status` returns the current build state and last-failure cargo output (full)
- [ ] Closing the supervisor↔engine TCP connection causes the engine to cancel all tasks via the cancel ladder, SIGKILL survivors, and exit
- [ ] Ids whose `<top>` references a retired engine return `not_found` without crossing the TCP boundary
- [ ] `run_task` returns a rendered task report on terminal status; `get_task` returns the same report for running or completed tasks
- [ ] `ctx.summary(s)` populates the report's Summary slot; absent summary falls back to `Last n lines:` tail
- [ ] `install_skills(target_dir)` and `rnme :install-skills <target>` both copy the embedded `docs/manual/` tree to `<target>/rnme/<skill>/`
- [ ] All RPC calls use `serde_json` JSONL framing (no embedded newlines, no pretty-print)
- [ ] `LogEntry.seq` is engine-global monotonic; `LogStore::push` stamps via `next_seq.fetch_add`

### Quality

- [ ] `cargo build` clean
- [ ] `cargo test` passes
- [ ] `cargo clippy` clean (no new warnings)
- [ ] Integration tests exercise: spawn-then-subscribe (no race), edit during running task (gen survives), build failure surfacing, cooldown retirement, kill_all behavior, supervisor disconnect cleanup

## Human Review Gates

| Gate | Where | Why | Auto-Approve? |
|---|---|---|---|
| **G0** Context synthesis | After Phase 0 | Surface unknowns about `rmcp` API shape, `notify` patterns, and any design holes the research uncovered | **Requires review** — research findings can shift later phases |
| **G1** Foundational changes review | After Phase 1 | LogStore seq migration changes a wire-relevant invariant (per-source → global seq); TUI must still render correctly | **Requires review** — affects the entire log subsystem and the TUI |
| **G2** Wire protocol freeze | After Phase 2 | Once the engine and supervisor are both speaking this protocol, changes touch both sides at once | **Requires review** — shape change later means dual-side rework |
| **G3** Engine daemon working | After Phase 3 | Confirm the `--engine` mode is exercisable standalone with a simple test harness before building the supervisor on top | Auto-approve if validator passes |
| **G4** Supervisor core working | After Phase 4 | Address routing, snapshot merging, multi-gen lifecycle — the highest-novelty slice | **Requires review** |
| **G5** Build state machine | After Phase 5 | File-watcher behavior + the LastBuildFailed semantics affect every spawn-shaped call | **Requires review** |
| **G6** MCP surface complete | After Phase 6 | Final API shape an agent will see; report rendering choices | **Requires review** |
| **G7** Skills content | After Phase 7 | Skill content is what teaches connecting agents; needs a human writer eye even if the install plumbing is auto-approvable | **Requires review** |
| **G8** Integration sign-off | After Phase 8 | Final acceptance | **Requires review** |

---

## Context

- **Design source:** `docs/mcp_design.md` (fully spec'd; this plan does not relitigate)
- **Engine reference:** `docs/runtime_engine_design.md`
- **Existing surface:**
  - `src/cli.rs` — has `--tui` / `--cli` mode flags; needs `--engine` and `--mcp` arms added
  - `src/execution/engine.rs` — `Engine`, `EngineHandle`, `GraphSnapshot`, `TaskNode`, `ProcessNodeInfo`
  - `src/execution/execution.rs` — `TaskExecution`, `TaskStatus`, `ProcessStatus`
  - `src/execution/control.rs` — `EngineError`, `KillSignal`, `SpawnOptions`
  - `src/log/store.rs` — `LogStore`; current `LogEntry.seq` is per-source, must move to engine-global
  - `src/log/buffer.rs` — `OutputBuffer`; currently allocates upstream seq
  - `src/log/filter` — `FilterExpr`, parser
  - `src/task.rs` — `Registry`, `TaskDef`, `TaskContext`
  - `src/builtin.rs` — where `:install-skills` lands
  - `src/bin/rnme/` — discovery + compile pipeline (touched only if discovery changes for the watcher)
- **New crate dependencies expected:** `rmcp` (official Rust MCP SDK), already-present `notify`, already-present `tokio_util`. May need `include_dir` for skill embedding.
- **Files to add (new):**
  - `src/mcp/mod.rs` — module root
  - `src/mcp/wire.rs` — wire types
  - `src/mcp/transport.rs` — JSONL framing helpers
  - `src/mcp/engine_server.rs` — `--engine` dispatch
  - `src/mcp/supervisor.rs` — `--mcp` dispatch + supervisor state
  - `src/mcp/routing.rs` — dotted address parser, snapshot merger
  - `src/mcp/build.rs` — build state machine + file watcher integration
  - `src/mcp/tools.rs` — MCP tool implementations
  - `src/mcp/report.rs` — task report renderer
  - `src/mcp/skills.rs` — embedded skill install
  - `docs/manual/rnme-operate/SKILL.md`, `docs/manual/rnme-author/SKILL.md` (+ supporting files)

---

## Team

| Name | Role | Type | Model | Strategy | Plan Approval |
|---|---|---|---|---|---|
| `mcp-lead` | Architect / coordinator (this is the lead seat — reviews implementor proposals, mediates cross-phase questions) | general-purpose | Opus | team | — |
| `r-engine-internals` | Researcher: TaskExecution lifecycle, status mutators, where to wire timestamps + summary | Explore | Opus | subagent | no |
| `r-logstore` | Researcher: LogStore current behavior, per-source seq usage, OutputBuffer seq allocation, callers of `subscribe_filtered` | Explore | Opus | subagent | no |
| `r-cli-dispatch` | Researcher: how `--tui` / `--cli` arms dispatch today, where `--engine` and `--mcp` should hook in | Explore | Opus | subagent | no |
| `r-rmcp-api` | Researcher: rmcp crate (docs.rs / GitHub) — server construction, tool registration, `instructions` slot, stdio transport | general-purpose | Opus | subagent | no |
| `r-notify-patterns` | Researcher: notify crate usage patterns, debouncing, watch-set rebuild on file appearance/disappearance | Explore | Opus | subagent | no |
| `r-builtin-tasks` | Researcher: how built-in tasks are registered (`src/builtin.rs`), how they receive arguments, how `:` prefix routing works | Explore | Opus | subagent | no |
| `synth` | Synthesizer: consolidate research findings, flag any design holes, produce a context brief for implementors | general-purpose | Opus | subagent | no |
| `i-logstore` | Implementor: LogStore engine-global seq migration + new range/grep/subscribe_with methods | general-purpose | Opus | team | yes |
| `i-task-execution` | Implementor: TaskExecution `summary` / `started_at` / `ended_at` + `ctx.summary()` + `TaskNode.summary` | general-purpose | Opus | team | yes |
| `i-task-info` | Implementor: `Registry::list_info() -> Vec<TaskInfo>` + `TaskInfo` struct | general-purpose | Sonnet | team | no |
| `i-wire` | Implementor: `src/mcp/wire.rs` — `WireMessage`, `Request`, `Response`, `Event`, `RpcError`, `CorrelationId`, `SubscriptionId`, `GrepScope`, serde derives on engine types | general-purpose | Opus | team | yes |
| `i-transport` | Implementor: `src/mcp/transport.rs` — JSONL framed send/recv helpers using `tokio_util::codec::LinesCodec` | general-purpose | Sonnet | team | no |
| `i-engine-server` | Implementor: `--engine` CLI arm + TCP listener + port-line handshake + per-connection request handler + subscription registry + EOF→cleanup path | general-purpose | Opus | team | yes |
| `i-routing` | Implementor: `src/mcp/routing.rs` — dotted address parser (`<u64>(\.<u64>(\.<u64>)?)?`), top-task→engine map, snapshot id rewriting | general-purpose | Opus | team | yes |
| `i-supervisor-core` | Implementor: `src/mcp/supervisor.rs` — `--mcp` arm, supervisor state, per-gen TCP client connections, in-flight correlation map, snapshot cache, generation lifecycle (spawn / promote-latest / cooldown / retire) | general-purpose | Opus | team | yes |
| `i-build-state` | Implementor: `src/mcp/build.rs` — file watcher with `notify`, debounce, RUNME.rs discovery for watch set, build state machine (Idle / Rebuilding / LastBuildFailed), spawn-shaped vs existing-state routing | general-purpose | Opus | team | yes |
| `i-mcp-tools` | Implementor: `src/mcp/tools.rs` — rmcp tool registrations and impls for every tool in design §"Tool surface" | general-purpose | Opus | team | yes |
| `i-report` | Implementor: `src/mcp/report.rs` — task report rendering (status / Stdout / Stderr / Events / Summary / Last n lines), format detection aggregation | general-purpose | Opus | team | yes |
| `i-skills` | Implementor: `src/mcp/skills.rs` — `include_dir!` embedding, atomic rename install, `install_skills` MCP tool, `:install-skills` builtin task | general-purpose | Sonnet | team | no |
| `i-skills-content` | Implementor: write the actual `docs/manual/rnme-operate/` and `docs/manual/rnme-author/` skill markdown content | general-purpose | Opus | team | yes |
| `v-logstore` | Validator: LogStore changes — TUI still renders, no per-source seq regression, `compose*` methods unchanged | Bash | Sonnet | subagent | no |
| `v-task-execution` | Validator: TaskExecution + ctx.summary tests | Bash | Sonnet | subagent | no |
| `v-wire` | Validator: wire types serialize/deserialize round-trip; no pretty-print leakage | Bash | Sonnet | subagent | no |
| `v-engine-server` | Validator: spin up `--engine`, parse port line, send a basic ListTasks/SpawnTask sequence over a test client | general-purpose | Sonnet | subagent | no |
| `v-supervisor` | Validator: supervisor↔engine smoke test with one gen, address parsing/routing, snapshot merge | general-purpose | Sonnet | subagent | no |
| `v-build-state` | Validator: file edit during running task → new gen, build failure surfacing, cooldown timer behavior | general-purpose | Sonnet | subagent | no |
| `v-mcp-tools` | Validator: full MCP surface, run_task end-to-end, get_logs cursor paging, get_task report rendering | general-purpose | Opus | subagent | no |
| `v-skills` | Validator: install_skills atomic replace, re-run idempotency, embedded tree matches `docs/manual/` | Bash | Sonnet | subagent | no |
| `v-integration` | Validator: full end-to-end agent flow simulation; asserts the design doc's headline scenarios (edit-during-running-task, kill_all, supervisor disconnect, build failure) | general-purpose | Opus | subagent | no |

**Team mix:** 30 named members (10 researchers/synthesizer, 13 implementors, 9 validators).
**Model mix:** Opus default; Sonnet only for `i-task-info`, `i-transport`, `i-skills`, and the per-implementor validators that just run cargo + grep output.

---

## Phases

### Phase 0 — Context research

Parallel researchers, each scoped to one question. Each writes findings into the **Findings** section of this plan; `synth` produces a consolidated brief that hangs from G0.

**Tasks:**

- [ ] `t0.r-engine-internals` (parallel) — *Find:* where `TaskStatus` transitions to terminal variants (`Done` / `Failed` / `Cancelled` / `Timeout`), the single mutator site if there is one. Identify exactly where to wire `started_at` (body-spawn site) and `ended_at` (terminal-status mutator). Note any existing fields on `TaskExecution` to avoid name collisions.
- [ ] `t0.r-logstore` (parallel) — *Find:* every reader of `LogEntry.seq`. Confirm `OutputBuffer` is the only seq allocator. Check whether `compose`, `compose_filtered`, `output`, `output_for`, `output_for_many`, `subscribe_filtered<F>` make any per-source-seq assumptions that the engine-global seq migration would break. Identify TUI's per-source rendering path that uses seq as a cursor.
- [ ] `t0.r-cli-dispatch` (parallel) — *Find:* the dispatch site for `--tui` and `--cli` in `src/cli.rs::run`. Document the function signature `cli::run` is called with from the generated runner binary. Identify the exact insertion point for `--engine` and `--mcp` arms and confirm clap rejects multiple mode flags.
- [ ] `t0.r-rmcp-api` (parallel) — *Fetch:* `rmcp` crate documentation (latest version on crates.io or docs.rs). Identify: how to construct an MCP server bound to stdio, how to register tools (sync vs async), how tool errors propagate, how the `instructions` field is set in the `initialize` response. Capture a minimal working example.
- [ ] `t0.r-notify-patterns` (parallel) — *Survey:* `notify` crate v7 API for recursive watching. Identify the recommended debounce pattern (manual debounce on top of `RecommendedWatcher`, or a debouncer crate). Check macOS FSEvents quirks. Capture a snippet that watches a set of files + rebuilds the watch set when files appear/disappear.
- [ ] `t0.r-builtin-tasks` (parallel) — *Find:* how `src/builtin.rs` defines tasks, how arguments arrive, how the `:`-prefix routing works in `Registry`. Identify the pattern to follow for `:install-skills <target>`.
- [ ] `t0.synth` (depends on all `t0.r-*`) — Consolidate findings into a "Context brief" section appended to this plan's Findings. Flag any design holes (questions the design doc didn't answer) for the human at G0.

**Gate G0:** Human reviews the Findings + Context brief; resolves design holes if any.

---

### Phase 1 — Foundational engine changes

Three independent slices. Each has a plan-approval step (implementor proposes, lead approves, then implements).

**Tasks:**

- [ ] `t1.i-logstore` (depends on `t0.synth`) — Migrate `LogStore` to engine-global `next_seq: AtomicU64`; `push` stamps via `fetch_add(1) + 1`; remove upstream `OutputBuffer` seq allocation. Add `get_range`, `grep`, `subscribe_with` methods exactly as specified in design §"LogStore changes". Existing methods unchanged.
- [ ] `t1.i-task-execution` (depends on `t0.synth`) — Add `summary: Mutex<Option<String>>`, `started_at: chrono::DateTime<Local>`, `ended_at: Mutex<Option<DateTime<Local>>>` to `TaskExecution`. Wire `started_at` at body-spawn (`execution.rs:371`, just before `tokio::spawn`). Wire `ended_at` at all three existing terminal-status writers — `execution.rs:382-399` (body completion: Done/Failed), `engine.rs:432` (cancel after body abort), `engine.rs:502` (timeout after body abort) — write status + `ended_at` together in the same critical section. Whether to consolidate behind a helper is the implementor's call. Add `TaskContext::summary(impl Into<String>)`; add `TaskNode.summary: Option<String>` (also `started_at`/`ended_at`) to the snapshot. `summary` writes publish a fresh snapshot.
- [ ] `t1.i-task-info` (depends on `t0.synth`) — Add `TaskInfo` struct (`name`, `group`, `description`, `args_help`) at `src/task.rs` (alongside the engine, not in the wire layer). Add `Registry::list_info() -> Vec<TaskInfo>` aggregating `TaskDef` metadata. Note: per design §"Single source of truth", this lives with the engine because it's an engine value type.
- [ ] `t1.v-logstore` (depends on `t1.i-logstore`) — `cargo build`, `cargo test`, run TUI manually with a multi-source task, confirm rendering still works. Confirm `get_range` / `grep` / `subscribe_with` round-trip.
- [ ] `t1.v-task-execution` (depends on `t1.i-task-execution`) — Test `ctx.summary()` last-write-wins; confirm `started_at` / `ended_at` populate; confirm `TaskNode.summary` flows to subscribers.

**Gate G1:** Lead approves implementor plans before each `t1.i-*` writes code; human reviews after `t1.v-*` complete.

---

### Phase 2 — Wire protocol + transport

**Tasks:**

- [ ] `t2.i-wire` (depends on G1) — Create `src/mcp/wire.rs`. Define every type listed in design §"Single source of truth: engine types ARE wire types" — `WireMessage`, `Request`, `Response`, `Event`, `RpcError`, `CorrelationId`, `SubscriptionId`, `GrepScope`. Add `Serialize`/`Deserialize` derives on engine types that don't have them (`GraphSnapshot`, `TaskNode`, `ProcessNodeInfo`, `TaskStatus`, `ProcessStatus`, `TaskId`, `KillSignal`, `SpawnOptions` — `LogEntry` already has `Serialize`, may need `Deserialize`). **Discipline:** do not introduce parallel "wire-only" copies of engine types; modify the engine types if needed.
- [ ] `t2.i-transport` (depends on `t2.i-wire`) — Create `src/mcp/transport.rs`. Two helpers: `send(&WireMessage) -> Result<()>` and `recv() -> Result<WireMessage>`, both wrapping `Framed<TcpStream, LinesCodec>`. Single discipline point — only this module calls `serde_json::to_string` (compact, never `to_string_pretty`). Unit test: round-trip every `WireMessage` variant.
- [ ] `t2.v-wire` (depends on `t2.i-wire` + `t2.i-transport`) — `cargo build`, `cargo test`, confirm round-trip serialization for every variant; confirm no pretty-print leakage (assert no `\n` mid-message).

**Gate G2:** Human reviews wire shapes and approves.

---

### Phase 3 — Engine daemon (`rnme --engine`)

**Tasks:**

- [ ] `t3.i-engine-server` (depends on G2) — Create `src/mcp/engine_server.rs` and add `--engine` arm to `src/cli.rs`. Behavior:
  - bind `127.0.0.1:0` via `tokio::net::TcpListener`
  - print `{"port": <u16>}` as a single line on stdout (the only stdout write — errors to stderr)
  - accept one supervisor connection; if a second arrives, refuse it
  - spawn an `Engine` with `start_task_id` from `--start-task-id N` (default 1)
  - request handler loop: parse `WireMessage::Request`, dispatch on `Request` variant, write `WireMessage::Response` with the same `CorrelationId`
  - subscription registry: `HashMap<SubscriptionId, FilterExpr>`; engine-allocated monotonic IDs per connection
  - event push: forward `watch::Receiver<GraphSnapshot>` to `WireMessage::Event::Graph`; forward filtered `broadcast::Receiver<LogEntry>` to `WireMessage::Event::Log` per active subscription
  - cleanup on EOF / write error: cancel all tasks via root-cancel, wait `kill_timeout`, SIGKILL survivors, drop `LogStore`, `exit(0)` (or `exit(1)` if any child resisted SIGKILL)
- [ ] `t3.v-engine-server` (depends on `t3.i-engine-server`) — Test client harness: spawn `target/debug/runme --engine`, parse port line, connect, send `ListTasks` → receive expected response; send `SpawnTask` → receive `{task_id, initial_seq}`; send `SubscribeLogs(from_seq=initial_seq)` → confirm log delivery; close connection → confirm engine exits within `kill_timeout`.

**Gate G3:** Auto-approve if `t3.v-engine-server` passes; otherwise surface to human.

---

### Phase 4 — Supervisor core (`rnme --mcp`)

Three implementors run partially in parallel — `i-routing` and `i-supervisor-core` overlap; `i-mcp-tools` waits on both because it needs the routing layer to dispatch through.

**Tasks:**

- [ ] `t4.i-routing` (depends on G3) — Create `src/mcp/routing.rs`:
  - `Address` parser for `<u64>(\.<u64>(\.<u64>)?)?` returning `(top, task, seq)`; `bad_request` on malformed input
  - `EngineMap`: `HashMap<TopTaskId, EngineRef>`; `lookup(top)` and `insert(top, engine_ref)`
  - Snapshot rewriter: walk a `GraphSnapshot`, identify each top-level ancestor, rewrite embedded ids to `"<top>.<task>"` strings on outbound
  - Merger: take `Vec<GraphSnapshot>` from all live gens, produce one flat list of top-level tasks (with subtrees) ordered by top-task id ascending. No supervisor-level meta-root.
  - `not_found` shortcut for retired-engine ids without crossing TCP
- [ ] `t4.i-supervisor-core` (depends on G3) — Create `src/mcp/supervisor.rs` and add `--mcp` short-circuit to **`src/bin/rnme/main.rs`** (NOT `src/cli.rs`):
  - **Architecture:** outer `rnme` checks `--mcp` *before* `compile_workspace()` and runs the supervisor directly in the outer process. No discover, no compile, no exec. The runner binary never sees `--mcp`.
  - From inside the supervisor, `std::env::current_exe()` returns the outer `rnme` binary. Spawning `Command::new(current_exe()).arg("--engine")` re-enters outer rnme, which transparently does discover+compile+exec into a runner with `--engine` (which becomes the engine daemon).
  - `Supervisor` state: `Vec<Generation>` (each holding a `tokio::process::Child`, a TCP write half, a snapshot watcher, an in-flight `HashMap<CorrelationId, oneshot::Sender>`, the latest cached `GraphSnapshot`, the open subscriptions); a `latest_gen: GenerationId` pointer
  - Per-gen connection task: read loop that demultiplexes `WireMessage::Response` (via correlation map), `WireMessage::Event::Graph` (updates cache, fires watch update), `WireMessage::Event::Log` (forwards to subscribers)
  - `spawn_engine(start_task_id)`: `tokio::process::Command::new(current_exe()).arg("--engine").arg("--start-task-id").arg(N).stdout(piped).stderr(piped).spawn()` — read first stdout line for port, connect TCP, register as gen
  - **Generation lifecycle (no cooldown):**
    - On port-line: promote new gen to "latest"; new spawns route there.
    - A gen with running tasks: never retire.
    - A gen whose tasks are all terminal AND is not latest: **stay alive indefinitely** for the lifetime of the supervisor — agent can keep querying its logs.
    - A gen that never had tasks (spawned, then immediately eclipsed by another rebuild before any spawn): **retire immediately** — no logs of value, no reason to keep the process around.
    - Latest gen: never retired by tasks completing.
  - Cleanup: on supervisor drop (MCP session ends), close all TCP connections; engines clean themselves up via EOF path.
  - Stderr-only `tracing-subscriber` install before `Supervisor::serve` (rmcp owns stdout). Lint sweep `src/mcp/` for stray `println!`/`eprintln!(_, "...")` to stdout.

  This task is the **highest-novelty slice** — propose plan, lead approves, then implement.
- [ ] `t4.v-supervisor` (depends on `t4.i-routing` + `t4.i-supervisor-core`) — Smoke test: launch supervisor with a single RUNME.rs containing one task; confirm gen 1 spawns; send `ListTasks` through supervisor → forwarded to gen 1 → response returned; send `SpawnTask` → confirm dotted address comes back; close connection → confirm engine cleans up.

**Gate G4:** Human reviews supervisor + routing.

---

### Phase 5 — File watcher + build state machine

**Tasks:**

- [ ] `t5.i-build-state` (depends on G4) — Create `src/mcp/build.rs`:
  - File watcher with `notify::RecommendedWatcher`, 200ms debounce. Use the supervisor-owned watcher pattern from `t0.r-notify-patterns` findings (NOT `src/watch.rs`).
  - Watch set: every discovered RUNME.rs + every sibling `.rs` file in each RUNME.rs's directory, respecting `.gitignore`. Rebuild watch set when discovery changes.
  - **Note:** the supervisor's discovery pass exists *only* to populate the watch set. The child engine does its own discovery internally during compile. Do not duplicate compile logic in the supervisor.
  - `BuildState` enum: `Idle`, `Rebuilding`, `LastBuildFailed { last_failure_output: String }`. Single-state, separate from per-engine lifecycle.
  - State transitions per design §"Build state machine".
  - Tool routing: spawn-shaped vs existing-state per design table.
  - On debounced event: spawn next gen via `Supervisor::spawn_engine(past_last_used_id)`; route new spawns there on success; retire old gen-with-no-tasks immediately on success (gens that had tasks stay alive — see Phase 4 lifecycle).
  - Capture child stderr into `last_failure_output` if child exits before printing port line; surface its head (~12 lines) on `spawn_task` / `run_task` errors.
- [ ] `t5.v-build-state` (depends on `t5.i-build-state`) — Tests:
  - File edit during running task → new gen spawns, old task keeps running, new spawns route to new gen.
  - Edit RUNME.rs to make compile fail → next `spawn_task` returns build error with cargo head; `get_build_status` returns full output; existing `get_logs` against running task still works.
  - **Persistence:** spawn task on gen 1, edit (gen 2 takes over), task completes — gen 1 stays alive — `get_logs` against gen 1's tasks continues to return data for the supervisor's lifetime. After supervisor exits, gen 1 cleans up via EOF.
  - **Never-had-tasks retirement:** spawn gen 1 with no tasks; edit immediately so gen 2 takes over before any task spawn — gen 1 retires immediately.
  - `kill_all` → cancels every direct child of the latest gen's root; old gens (with already-terminal tasks) are unaffected and stay queryable.
  - Vim-style atomic-save (Remove + Create rather than Modify) triggers a rebuild — `is_meaningful_event` accepts Remove.

**Gate G5:** Human reviews build behavior. This is where the agent UX lives or dies.

---

### Phase 6 — MCP tool surface + report renderer

`i-mcp-tools` and `i-report` run in parallel — they touch different files and the tools `use` the report module, but the surfaces don't overlap during implementation.

**Tasks:**

- [ ] `t6.i-mcp-tools` (depends on G5) — Create `src/mcp/tools.rs`:
  - rmcp server construction; stdio transport; tool registry
  - Implement every tool in design §"Tool surface": `run_task`, `list_tasks`, `spawn_task`, `kill_task`, `kill_process`, `kill_all`, `get_graph`, `get_task`, `get_logs`, `grep_logs`, `get_build_status`, `install_skills`
  - `run_task` is supervisor-implemented (not an engine RPC primitive): `spawn_task` → `subscribe_logs(from_seq=initial_seq)` into a tail buffer → await graph event with terminal status → call report renderer → return
  - `instructions` slot in `initialize` response: short blurb describing available skills + how to install them via `install_skills(target_dir)`
  - Each tool's input schema: dotted-address ids accepted as strings; `bad_request` on parse failure
- [ ] `t6.i-report` (depends on G5; can run parallel to `t6.i-mcp-tools`) — Create `src/mcp/report.rs`:
  - `render(handle: &EngineHandle, top_id: TaskId, tail_n: usize) -> String`
  - Format per design §"Format" exactly: header line, Started/Run-time, Stdout/Stderr/Events line counts + format detection (`JSON 91%` style, omit if no kind clears 60%), Summary slot, Last n lines fallback
  - Status formatting: `completed (exit 0)`, `failed: <reason>`, `cancelled`, `timed out`, `running (setup)`, `running (ready)`, with `(running)` suffix on Run time when non-terminal
  - Walk `LogStore` entries for `source_ids_for(top_id)`, count on demand (no new state)
- [ ] `t6.v-mcp-tools` (depends on `t6.i-mcp-tools` + `t6.i-report`) — End-to-end MCP harness: connect a stdio MCP test client; exercise every tool; confirm `run_task` blocks until terminal and returns rendered report; confirm `get_task` works on running tasks; confirm cursor paging on `get_logs`.

**Gate G6:** Human reviews final agent-facing API.

---

### Phase 7 — Skills bundle

Mostly independent — can land any time after Phase 0, but blocks G7. Run in parallel with Phases 4–6.

**Tasks:**

- [ ] `t7.i-skills` (depends on G0; can start after Phase 0) — Create `src/mcp/skills.rs`:
  - `include_dir!("docs/manual")` (or equivalent) embeds the tree
  - `install_to(path: &Path) -> Result<InstallReport>`: write to sibling temp dir, atomic rename over `<target>/rnme/`, return canonical absolute path + list of installed skill names
  - `install_skills(target_dir)` MCP tool wraps `install_to`
  - `:install-skills <target>` builtin task in `src/builtin.rs` also wraps `install_to`
  - Concurrent calls serialize through a supervisor-side mutex
  - Errors: `target path is a file`, `permission denied creating <path>`, etc., as single-string MCP failures
- [ ] `t7.i-skills-content` (depends on G0; can start after Phase 0) — Write actual skill content:
  - `docs/manual/rnme-operate/SKILL.md` — frontmatter (name + description trigger string) + body teaching the MCP primitives an agent uses to drive tasks
  - `docs/manual/rnme-author/SKILL.md` — frontmatter + body teaching RUNME.rs idioms (`cmd!`, args/clap, frontmatter `[dependencies]`, readiness, group structure, file placement)
  - Sibling files for progressive disclosure as the body needs them
- [ ] `t7.v-skills` (depends on `t7.i-skills` + `t7.i-skills-content`) — Validation:
  - `install_skills` to a tempdir; confirm tree at `<target>/rnme/<skill>/SKILL.md`
  - Re-run install; confirm idempotent (atomic rename)
  - Manual hand-edit a file inside `<target>/rnme/`; re-run; confirm overwrite
  - Confirm `rnme :install-skills <target>` produces same result

**Gate G7:** Human reviews skill content (the prose matters).

---

### Phase 8 — Integration tests

**Tasks:**

- [ ] `t8.v-integration` (depends on G6 + G7) — End-to-end tests covering the design doc's headline scenarios:
  - Spawn-then-subscribe race (initial_seq mechanism)
  - Edit RUNME.rs during running task → old gen survives, new spawns route to new gen
  - Build failure on edit → spawn_task returns cargo head error; existing get_logs works
  - kill_all → all non-latest retire, latest survives task-less
  - Supervisor disconnect → engine cleans up within kill_timeout
  - Cooldown TTL: access resets timer; expiry retires gen
  - Stale id (top-task in retired engine) returns not_found
  - Skill install + re-install idempotency
  - run_task report contains expected lines; ctx.summary populates Summary slot

**Gate G8:** Final human acceptance. All acceptance criteria checked.

---

## Validation Profile

```yaml
validation:
  build:
    command: "cargo build"
    required: true
  tests:
    command: "cargo test"
    required: true
  clippy:
    command: "cargo clippy --all-targets -- -D warnings"
    required: true
  manual:
    description: "Per-phase manual smoke tests as specified in each phase's validator task"
    required: true
```

---

## Findings

*(Populated during Phase 0 by researchers; consolidated by `synth`.)*

### `t0.r-engine-internals`

**1. Terminal-status mutator sites (NOT a single site — there are three).** They must all be updated together (or, preferably, consolidated behind a single helper) when wiring `ended_at`:

- **Body completion (Done / Failed)** — `src/execution/execution.rs:382-399`, inside the `tokio::spawn` block in `TaskExecution::spawn_body`. After the body future resolves:
  ```rust
  let mut s = task_status.lock().await;
  match &result {
      Ok(()) => { *s = TaskStatus::Done; }
      Err(task_err) => {
          let failure = TaskFailure { ... };
          *s = TaskStatus::Failed(failure);
      }
  }
  ```
  Followed by `eng.publish_snapshot().await` (line 402).

- **Cancellation** — `src/execution/engine.rs:428-434` in `EngineInternals::cancel_task_with`:
  ```rust
  let mut s = exec.task_status().lock().await;
  if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
      *s = TaskStatus::Cancelled;
  }
  ```
  Followed by `self.publish_snapshot().await` (line 436).

- **Timeout** — `src/execution/engine.rs:499-504` in `EngineInternals::timeout_task`:
  ```rust
  let mut s = exec.task_status().lock().await;
  if matches!(*s, TaskStatus::Setup | TaskStatus::Ready) {
      *s = TaskStatus::Timeout;
  }
  ```
  Followed by `self.publish_snapshot().await` (line 505).

`TaskStatus::Ready` (non-terminal) is also written from `task.rs` (`bind_ready` line 731 and `mark_ready` line 756). Don't write `ended_at` for those.

**2. Body `tokio::spawn` site (where `started_at` should be set).** `src/execution/execution.rs:371` — the `let handle: JoinHandle<TaskResult> = tokio::spawn(async move { ... })` inside `TaskExecution::spawn_body`. Wire `started_at = Local::now()` synchronously immediately before this `tokio::spawn` call so the field is populated before `spawn_child` returns the `TaskHandle` and before the first `publish_snapshot` runs. The synthetic root also goes through `spawn_body` (called from `Engine::start` at `engine.rs:809`), so the root gets `started_at` for free.

**3. Existing `TaskExecution` fields (collision check).** Defined in `src/execution/execution.rs:187-261`. None collide with `started_at`, `ended_at`, or `summary`. Constructor `TaskExecution::with_log_store_and_engine` at `execution.rs:269-309` is the only place that initialises every field; either seed `started_at` with a placeholder or make it `Option<DateTime<Local>>` initialised to `None` until `spawn_body` overwrites it.

**4. Snapshot publish path.** `src/execution/engine.rs:251-287` — `EngineInternals::publish_snapshot`. Walks `self.table`, locks each `TaskExecution`'s `task_status`, `children`, and `processes`, builds `TaskNode`s, broadcasts via `self.graph_tx.send(snapshot)` (a `tokio::sync::watch::Sender<GraphSnapshot>`). Already called from: body completion (`execution.rs:402`), cancel ladder (`engine.rs:436`), timeout (`engine.rs:505`), `spawn_child` (`engine.rs:358-360`), `monitor_spawns` events (`engine.rs:554-556`, `570-572`, `598-600`). For `summary`: `ctx.summary(s)` should set `TaskExecution.summary` (last-write-wins via the `Mutex<Option<String>>`) and then call `engine.publish_snapshot().await` through the `Weak<EngineInternals>` already wired into `TaskContext` at `task.rs:196`. This mirrors how `bind_ready`/`mark_ready` reach into the engine today.

**5. Where `TaskNode` is built from `TaskExecution`.**

- `TaskNode` struct: `src/execution/engine.rs:174-185`. Fields: `id, name, parent, children, status, processes`.
- Construction site: `src/execution/engine.rs:267-278`, inside `publish_snapshot`. Single place to copy `started_at`, `ended_at`, and `summary` from `TaskExecution` onto `TaskNode`. `started_at` can be a plain field (`DateTime<Local>`); `ended_at` and `summary` need to lock their `Mutex` and clone the inner `Option`.

CLI/TUI consumers of `TaskNode` (`cli.rs:255-258, 399-402`, `tui/sidebar.rs:603-607`, `tui/app.rs`) only read `status`/`children`/`processes` — adding new fields to `TaskNode` is non-breaking for them.

**Recommended wire-in points:**

| New field | Wire-in site |
|---|---|
| `started_at: DateTime<Local>` | Set in `TaskExecution::spawn_body` immediately before the `tokio::spawn` at `execution.rs:371`. Snapshot copy in `publish_snapshot` at `engine.rs:267-278`. |
| `ended_at: Mutex<Option<DateTime<Local>>>` | Three writers, one per terminal mutator: `execution.rs:384-399`, `engine.rs:428-434`, `engine.rs:499-504`. Each writes `ended_at` together with the status transition under the same critical section (or via a shared helper like `mark_terminal(&self, status: TaskStatus)`). Snapshot copy in `publish_snapshot`. |
| `summary: Mutex<Option<String>>` | Written by a new `TaskContext::summary(s)` method (in `src/task.rs`) that locks the field, then calls `engine.publish_snapshot().await` through the existing `engine: Option<Weak<EngineInternals>>` at `task.rs:196`. Snapshot copy in `publish_snapshot`. |

**Refactor suggestion (flagged for `synth`/G0):** Introduce `EngineInternals::mark_terminal(&self, exec: &Arc<TaskExecution>, status: TaskStatus)` (or a method on `TaskExecution`) that writes status + `ended_at` together and calls `publish_snapshot`. Then the body-completion block in `spawn_body`, `cancel_task_with`, and `timeout_task` can all funnel through one site, eliminating three-way drift risk. **Implementor `i-task-execution` should propose whether to consolidate or update in place.**

**⚠ Note for orchestrator:** The `Explore` subagent type cannot edit files or call TaskUpdate. Worker placed findings inline; orchestrator wrote them here and marked the task complete. Future Phase 0 researchers using `Explore` will hit the same constraint.

### `t0.r-logstore`

**Readers (every `LogEntry.seq` read site):**

- `src/log/store.rs:90` — `compose()` sort: `a.seq.cmp(&b.seq)`. Assumes engine-global monotonicity; today only "happens to work" because per-source seqs are also assigned in temporal order. **Cleaner under migration.**
- `src/log/store.rs:101` — `compose_owned()` sort. **Cleaner.**
- `src/log/store.rs:117` — `compose_filtered()` sort. **Cleaner.**
- `src/log/store.rs:287` — `output_for_many()` historical-snapshot sort. **Cleaner.**
- `src/log/store.rs:524` — test asserts `filtered[0].seq < filtered[1].seq` — holds under both schemes.
- `src/log/stream.rs:352, 432, 519, 523, 591` — tests with fixture seqs / numeric uses. **Safe.**
- `src/process.rs:1501-1503` — `test_log_entry_source_and_seq` asserts `entries[0..3].seq == 0,1,2`. **At risk.**
- `src/tracing_layer.rs:240` — `assert_eq!(entry.seq, 0)`. **At risk.**
- `src/tracing_layer.rs:270-273` — asserts `entries[0..4].seq == 0,1,2,3`. **At risk — will fail because tracing layer pushes into bare `OutputBuffer` (no LogStore stamping path); all four entries will share the default seq.**
- `src/log/extract.rs:681` — `assert_eq!(entry.seq, 0)` on a manual fixture. **Safe.**

**Allocators / pushers (sites that set seq):**

Two real allocators today:
- `src/process.rs:605` — `let mut seq: u64 = 0;` for `exec()` stdout/stderr pipeline; `build_log_entry` increments and stamps via `seq: &mut u64`. Per-process counter. **Must stop allocating; let LogStore stamp.**
- `src/tracing_layer.rs:42, 48, 182` — `LogEntryLayer { seq: AtomicU64 }`, `self.seq.fetch_add(1, ...)` per event. Per-layer counter. **Must stop allocating.**

Synthetic / placeholder seq sites (fixtures or `seq: 0` default — already aligned):
- `src/log/mod.rs:123` — `LogEntry::raw()` sets `seq: 0`. Used by `TaskContext::println`. **Already correct.**
- `src/process.rs:1274, 1309` — test fixture pushes.
- `src/tui/render.rs:390`, `src/tui/keys.rs:1322`, `src/tui/app.rs:584`, `src/tui/viewport.rs:523`, `src/tui/event.rs:782` — TUI test fixtures only.
- `src/log/extract.rs:671`, `src/log/search.rs:315, 330`, `src/log/filter/mod.rs:42, 73` — test fixtures.
- `src/log/store.rs:362, 378` — test fixtures.
- `src/log/stream.rs:208, 226, 246` — test fixtures.

**Crucially: `OutputBuffer::push` does NOT allocate seq.** The design's "OutputBuffer's upstream seq allocation goes away" refers to the per-process `let mut seq: u64 = 0;` at `process.rs:605` flowing through `build_log_entry` (`process.rs:243-260`). `OutputBuffer` itself is a passthrough.

**Public surface audit:**

- `compose()` / `compose_filtered()` / `compose_owned()` — **safe** (sort is correct either way).
- `output()` / `output_for(source)` / `output_for_many(sources)` — **safe**. Snapshot via `compose()` then live-forward via `subscribe()`.
- `subscribe()` / `subscribe_filtered<F>` — **safe**.
- `source_entries(TaskId)` — **safe**. Returns slice; insertion order = push order = monotonic global seq order within a single source.
- `source_ids()` / `group_by_source()` — **safe** (no seq use).
- `push()` (store.rs:55) — **needs migration**. New: stamp `entry.seq = next_seq.fetch_add(1) + 1` *before* `tx.send(entry.clone())` and storage. Capacity-eviction policy ("drop oldest from largest source") still works since within-source order remains globally-monotonic.
- `ingest_buffer()` (store.rs:78) — **needs review**. Today it forwards entries with whatever seq the buffer carries. Under new scheme, those entries should be re-stamped via `push()`, since buffer entries no longer have meaningful seq.

**TUI cursor usage:**

- TUI's viewport (`src/tui/viewport.rs`) uses `cursor: usize` — a **positional index into a Vec of entries**, not a seq value. See `ScrollState::Pinned { cursor: usize, top: usize }` (line 25-29). All scroll/page operations use array indices.
- No TUI module reads `LogEntry.seq` for navigation. Only `seq` references in `src/tui/` are test-fixture inits.
- **Within a single source, since `LogStore::push` will use `fetch_add` and serialized push order yields strictly increasing seq, an entry's seq is monotonically increasing within its source bucket** — existing in-source order preserved. **TUI's per-source rendering needs no migration.**

**Tests at risk:**

- `src/process.rs:1501-1503` (`test_log_entry_source_and_seq`) — asserts `entries[i].seq == i`. Under migration, `exec()` no longer allocates seq; entries reach `OutputBuffer` directly with `seq: 0`. Assertion fails unless test goes through a `LogStore` or asserts only strict monotonicity.
- `src/tracing_layer.rs:240` (`test_tracing_layer_captures_info_with_fields`) — single-entry `seq == 0`. Might still pass coincidentally since default is 0; but semantic is wrong.
- `src/tracing_layer.rs:270-273` (`test_tracing_layer_multiple_levels`) — asserts `entries[0..4].seq == 0,1,2,3`. **Will fail** — tracing layer pushes into bare `OutputBuffer` (no LogStore stamping path); all four entries share default seq.
- `src/log/store.rs:429` (`test_compose_same_seq_deterministic`) — explicitly tests "two entries with same seq from different sources." Under new scheme, **same seq from different sources is impossible**. Test loses meaning; remove or repurpose.

**Additional notes:**

- `LogEntry::raw()` at `src/log/mod.rs:118` hard-codes `seq: 0`, used by `TaskContext::println()`. Aligned with new scheme. Confirm `println` path actually flows through `LogStore::push` rather than directly into a buffer.
- Three new methods to add per design (`get_range`, `grep`, `subscribe_with`) are additive, no conflict with existing surface.
- `src/log/store.rs:22` capacity-eviction comment "(by seq within each source)" stays accurate — global seq is still ordered within a source.

### `t0.r-cli-dispatch`

**Dispatch site.** Single entry: `cli::run` in `src/cli.rs:88-181`. Parses `RnmeArgs`, decides `UiMode`, branches in two places:

1. "No task specified" branch (rest empty), lines 93–116: only handles `UiMode::Tui`, errors otherwise.
2. "Task specified" branch (lines 118–180): parses `--timeout`, then `match ui { UiMode::Tui … UiMode::Cli … UiMode::Agent … }` (lines 151–180).

For `--engine`/`--mcp`, short-circuit *before* either match — neither needs `args.rest`, and `--engine` doesn't resolve a task name:

```rust
pub async fn run(registry: Arc<Registry>, group_names: HashMap<String, String>) {
    let args = RnmeArgs::parse();

    if args.engine {
        crate::mcp::engine_server::run(registry, args.start_task_id.unwrap_or(1)).await;
        return;
    }
    if args.mcp {
        crate::mcp::supervisor::run().await;   // supervisor doesn't use registry/group_names
        return;
    }

    let has_terminal = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let explicit_ui = args.explicit_ui_mode();
    // … existing TUI/CLI/Agent dispatch unchanged …
}
```

Supervisor takes no `registry`/`group_names` because per design §"Topology"/§"Change detection", the supervisor's *children* (engines) do their own discover+compile internally — supervisor only watches files. `cli::run` still receives them from the generated runner (it has to; this binary IS a runner), but the supervisor arm just ignores them.

**`RnmeArgs` additions.** Mode flags must be mutually exclusive; update each existing `conflicts_with_all` to enumerate all peers:

```rust
#[derive(Parser)]
#[command(name = "runme")]
pub struct RnmeArgs {
    #[arg(long, conflicts_with_all = ["cli", "engine", "mcp"])]
    pub tui: bool,

    #[arg(long, conflicts_with_all = ["tui", "engine", "mcp"])]
    pub cli: bool,

    /// Run as headless engine daemon (TCP JSONL on 127.0.0.1:0).
    /// Prints `{"port": <u16>}` on stdout and accepts a single supervisor connection.
    #[arg(long, conflicts_with_all = ["tui", "cli", "mcp"])]
    pub engine: bool,

    /// Run as MCP server on stdio (manages child engine generations).
    #[arg(long, conflicts_with_all = ["tui", "cli", "engine"])]
    pub mcp: bool,

    /// Starting TaskId counter for --engine (defaults to 1).
    #[arg(long, requires = "engine")]
    pub start_task_id: Option<u64>,

    // ... existing format/timeout/filter/rest unchanged ...
}
```

Notes:
- `conflicts_with_all` is symmetric in clap v4; explicit listing is safer/readable. Clap rejects multiple mode flags at parse time, satisfying the design requirement.
- `requires = "engine"` makes `--start-task-id` only valid when `--engine` is set.
- `explicit_ui_mode()` does not need to change — `--engine`/`--mcp` are not UI modes.
- **Also flagged for `i-supervisor-core`:** add `--gen-cooldown` flag to `RnmeArgs` with `requires = "mcp"` (same shape) when supervisor lands. Design §"Generations" specifies it (default 15min).

**`cli::run` signature & call chain.**

```rust
pub async fn run(registry: Arc<Registry>, group_names: HashMap<String, String>) { … }
```

Chain from user's `rnme` to this function:

1. `src/bin/rnme/main.rs` is the *outer* driver. Does `--init` handling, `discover()`, `compile_workspace()`, then `exec::execvp`s the compiled runner binary at `compiled.binary_path`. Outer `rnme` does *not* call `cli::run` itself.
2. Runner binary's `main` is generated by `generate_runner_main` in `src/bin/rnme/codegen.rs:22-81`. Generated `main()` calls `__rnme_link()`, builds tokio runtime, runs init hooks, builds `Registry::from_inventory()`, registers dynamic tasks, then calls `rnme::cli::run(registry, group_names_owned).await`.
3. `cli::run` parses `RnmeArgs`, dispatches.

Implication:
- `--engine` runs *inside* the runner binary, so it has the registry + group_names that `--cli`/`--tui` see. `Registry::list_info()` (added in `t1.i-task-info`) reads from this. No new plumbing.
- `--mcp` is conceptually one level *up*. The runner binary it's running in was just compiled from current RUNME.rs files — fine, since at startup the supervisor doesn't need to introspect tasks. Registry/group_names received by supervisor are unused and discarded.

**Re-invocation pattern for the supervisor.**

```rust
let exe = std::env::current_exe()?;
let mut child = tokio::process::Command::new(&exe)
    .arg("--engine")
    .arg("--start-task-id").arg(start_id.to_string())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()?;
```

Subtleties for `i-supervisor-core`:

1. **`current_exe()` returns the *runner* binary, not the outer `rnme`.** When user runs `rnme --mcp`, outer `main.rs` `execvp`s into compiled runner (see `main.rs:85`). After exec, `current_exe()` is the runner's path inside the cache dir. Re-spawning *that* with `--engine` is exactly what we want — same artifact compiled from current RUNME.rs files, has registry baked in via inventory, parses `--engine`/`--start-task-id` via same `RnmeArgs`. **No need to track back to the outer `rnme` and re-run discover/compile from inside the supervisor.** Design §"Topology" says "spawn next `rnme --engine` (which compiles itself)" — except the runner is already compiled, so no recompile happens; only on the *next* generation does the cycle begin: file edit → debounce → supervisor invokes outer `rnme` (or equivalent) which runs discover+compile+exec again.

   **Open question:** is the supervisor supposed to invoke `current_exe()` (already-compiled runner) or the outer `rnme` driver path that runs discover+compile? Design strongly suggests outer-driver target so new gen reflects edited code. `current_exe()` after exec gives the runner, not the outer driver.

2. **Argv[0] vs `current_exe`.** On macOS `current_exe()` resolves the exec'd binary's actual on-disk path — after `execvp`, that's the cache-dir runner. To re-spawn the *outer* `rnme` (so discover/compile runs again with current file state), supervisor needs a path to the user-installed `rnme`. Options: (a) capture argv[0] in `outer rnme/main.rs` and pass via env var (e.g. `RNME_DRIVER_PATH`) before `execvp`; (b) require user's `rnme` on `PATH` and spawn `Command::new("rnme")`; (c) accept `current_exe()` and have supervisor not trigger recompile on its own — but then edited code never reaches a new gen, contradicting design.

   Recommended: option (a). Outer `main.rs` already sets `RNME_CACHE_DIR` and `RNME_RUNME_FILES` env vars before `execvp` (lines 69–72). Adding `RNME_DRIVER_EXE = std::env::current_exe()?` is a one-line change. Supervisor then does `Command::new(env::var("RNME_DRIVER_EXE")?)`.

3. **stdout/stderr piping.** `stdout(piped)` required to read `{"port": N}` line. `stderr(piped)` so on early child exit (compile fail / panic) supervisor captures cargo's output for `LastBuildFailed.last_failure_output`. Use tokio `BufReader` with `read_line` / `read_to_end`.

4. **No `stdin(null)` strictly required, but explicit pipe to null prevents accidental input.

5. **Process group / signal handling.** Per design §"Engine cleanup on disconnect", the engine cleans itself via TCP EOF. Supervisor doesn't need to signal child engine; closing TCP is sufficient. `tokio::process::Child` handle held only for `wait()`/reaping.

**Where registry + group_names come from at the dispatch site.** From parameters passed by codegen.rs:74 in the generated runner main: `Registry::from_inventory()` + dynamic tasks from init hooks + group_names from GroupDef inventory + InitContext overrides.

For `--engine`: engine daemon needs `registry` for `ListTasks`/spawning, and arguably `group_names` so `TaskInfo.group` carries human-readable group name. Both in scope — just pass into `engine_server::run`.

For `--mcp`: supervisor doesn't use them; engine *child* gets a fresh registry from its own runner main.

**Open questions flagged for synth/G0:**

1. **Outer-driver path for re-invocation (Subtlety #2).** Add `RNME_DRIVER_EXE` env var in outer `main.rs`, or rely on `current_exe()` (runner, not driver)? **Recommendation: env-var route.** Worth confirming at G3/G4.
2. **`--gen-cooldown` flag** missing from this research's `RnmeArgs` sketch — should be added when `i-supervisor-core` lands.
3. **Help-text visibility.** Should `--engine`/`--mcp` be `hide = true` to keep user-facing `--help` clean, or visible? Design treats them as sibling entry points — visible probably wins.

### `t0.r-rmcp-api`

**Version pinned:** `rmcp = "1.6.0"` (root `Cargo.toml`). Local source: `/Users/dgrijalva/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rmcp-1.6.0/` (cargo home is sandbox-restricted; readable via rendered HTML at `/Users/dgrijalva/Code/runme/target/doc/src/rmcp/*.html` and `/Users/dgrijalva/Code/runme/target/doc/rmcp/*`).

**Default features for the `server` use case:** rmcp default-features = `server` + `macros`. We need to **add** `transport-io` (gives `rmcp::transport::stdio` / `rmcp::transport::io`) and `schemars` (turns on `#[cfg(feature = "schemars")]` on the model types and is required for `JsonSchema` derivation on tool params). Cargo.toml today has `rmcp = "1.6.0"` only — **must extend `features = ["server", "macros", "transport-io", "schemars"]`** before this can compile. (Open question: is `schemars` already pulled by `macros`? README implies `schemars` is a separate feature flag.)

**Server construction (stdio):**

```rust
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = Supervisor::new(/* engine handle, etc */).await?;
    // `Supervisor: ServerHandler`; ServiceExt::serve takes any IntoTransport.
    // `stdio()` returns a (tokio::io::Stdin, tokio::io::Stdout) pair which is
    // an `IntoTransport<RoleServer, _, _>` via the AsyncRead+AsyncWrite blanket.
    let running = supervisor.serve(stdio()).await?;     // returns RunningService
    running.waiting().await?;                            // blocks until peer disconnects / ct fires
    Ok(())
}
```

Alternative: `rmcp::serve_server(service, transport)` is a free function with the same effect; for our supervisor the trait-method form is cleaner.

`RunningService` exposes `.peer()` (a `Peer<RoleServer>` for sending notifications back to the agent — useful for `notifications/tools/list_changed` when generations roll over), `.cancellation_token()` (for graceful shutdown), `.cancel().await` and `.waiting().await -> QuitReason`.

**Tool registration (macro path — recommended):**

The supervisor is a single-purpose tools-only server, so the all-macro path applies. `#[tool_router(server_handler)]` on the impl block emits both the router *and* the `ServerHandler` impl, leaving us only the `get_info()` override to write — but because we *do* need a custom `get_info()`, we use the explicit two-macro split.

```rust
use rmcp::{
    tool, tool_router, tool_handler, schemars,
    handler::server::wrapper::{Parameters, Json},
    model::{CallToolResult, Content, ErrorData as McpError, ErrorCode,
            ServerInfo, ServerCapabilities, Implementation},
    service::RequestContext, RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpawnTaskParams {
    /// Dotted task name, e.g. `web` or `web:server`.
    pub name: String,
    /// Optional positional/named args forwarded to the task.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional spawn-side timeout, seconds.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Tail this many log lines into the result.
    #[serde(default)]
    pub tail_n: Option<usize>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SpawnTaskOutput {
    pub task_id: String,
    pub status: String, // "pending" | "running" | "ready" | "failed"
    pub generation: u64,
}

#[derive(Clone)]
pub struct Supervisor { /* Arc<SupervisorInner> inside */ }

#[tool_router]
impl Supervisor {
    /// Spawn a task by name. Returns the assigned `task_id` and current status.
    #[tool]
    async fn spawn_task(
        &self,
        Parameters(params): Parameters<SpawnTaskParams>,
        // RequestContext is auto-extracted; gives us peer + ct (cancellation).
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<SpawnTaskOutput>, McpError> {
        let task = self.lookup(&params.name).ok_or_else(|| McpError::new(
            ErrorCode(-32004),                       // app-defined: not_found
            format!("task not found: {}", params.name),
            Some(serde_json::json!({ "code": "not_found" })),
        ))?;
        // … honor params.timeout, await spawn, optionally tail logs …
        let res = self.spawn(task, params.args, ctx.ct.clone()).await
            .map_err(|e| McpError::new(
                ErrorCode::INTERNAL_ERROR,
                e.to_string(),
                Some(serde_json::json!({ "code": "spawn_failed" })),
            ))?;
        Ok(Json(SpawnTaskOutput {
            task_id: res.id.to_string(),
            status: res.status.to_string(),
            generation: res.generation,
        }))
    }
}

#[tool_handler]
impl ServerHandler for Supervisor {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(self.skills_instructions()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "rnme".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}
```

Key facts about the macros:
- `#[tool]` derives a `Tool` (name + description + JSON Schema) at compile time. **Description comes from the function's `///` doc comment** (matches our project convention; do **not** use `description = "..."`). Name defaults to function name; override via `#[tool(name = "spawn_task")]` if we want a different surface name.
- Input schema is auto-derived from `Parameters<T>` where `T: Deserialize + JsonSchema`. Output schema is auto-derived from the return wrapper: `Json<T>` produces structured-content output with schema from `T: Serialize + JsonSchema`. Returning a bare `String` produces unstructured text content. Returning `CallToolResult` gives full control.
- Tool handlers can be **sync or async** — the macro detects `async fn` and wraps it accordingly. Both compile to a `CallToolHandler` trait impl that produces a `MaybeBoxFuture<Result<CallToolResult, ErrorData>>`.
- `RequestContext<RoleServer>` (or `ToolCallContext`) can be added as a parameter and is auto-extracted; that's how we get `ctx.ct` (cancellation token) and `ctx.peer` (for sending notifications).

**Manual / "trait-based" path** (for tools spread across files): each tool is a unit struct implementing `ToolBase` + `SyncTool<Server>` or `AsyncTool<Server>`, then registered via `ToolRouter::new().with_async_tool::<MyTool>()`. We probably don't need this for the supervisor — keep all six tools in one impl.

**Tool error propagation:** Two distinct mechanisms, and **the choice matters for the design.**

1. **Return `Err(McpError)` from the handler** → surfaces as a JSON-RPC error response (`-32xxx` code), i.e. a protocol-level error. Use this for malformed/invalid requests, unknown tool names, etc. `McpError::new(ErrorCode, message, Option<Value>)` — the `Value` is the structured `data` payload. Standard codes available: `INVALID_REQUEST` (-32600), `METHOD_NOT_FOUND` (-32601), `INVALID_PARAMS` (-32602), `INTERNAL_ERROR` (-32603), `RESOURCE_NOT_FOUND` (-32002), `PARSE_ERROR` (-32700). Custom codes (any `i32`) work fine via `ErrorCode(-32004)` — the spec reserves -32000..-32099 for app-defined; `ErrorCode` is a transparent newtype.

2. **Return `Ok(CallToolResult { is_error: true, … })`** → tool *executed* but produced an error result. The agent gets `is_error: true` plus content. Construct with `CallToolResult::error(vec![Content::text("…")])` or `CallToolResult::structured_error(value)`.

**Recommendation matching design §"Errors":**
- Bad request shape (missing/invalid args, unknown task name) → `Err(McpError::invalid_params(...))` with `data: { "code": "bad_request" | "not_found" }`.
- Tool *succeeded as a tool* but the underlying action failed (e.g. build error with cargo head, task crashed) → `Ok(CallToolResult::structured_error(json!({ "code": "build_failed", "head": "…", "exit_code": 101 })))`. This is what most agent SDKs render naturally and the design doc explicitly calls for `is_error: true` + structured payload.

Adding the `code` field via `data` (or via the structured payload) is how we attach `not_found`/`bad_request`/`build_failed` per the design.

**The `instructions` slot:**

`InitializeResult` (alias `ServerInfo`) has a public `instructions: Option<String>` field. The default `ServerHandler::initialize` returns `self.get_info()`, so we override `get_info()` (see code sketch above). Either set the field directly in the struct literal or use the builder method `ServerInfo::default().with_instructions("…")`.

The `instructions` text is what the design's "Discovery from MCP" section means — load it from `docs/manual/MCP_INSTRUCTIONS.md` (or `include_str!` it) and inject. This is exactly the standard MCP `initialize.result.instructions` field — agents (Claude Code, Continue, etc.) surface it in their system context.

The two-macro split (`#[tool_router]` + explicit `#[tool_handler] impl ServerHandler { fn get_info(...) }`) is documented in `handler/server/router/tool.rs` and is the right pattern when we need to override any default `ServerHandler` method.

**Shared state pattern:**

The supervisor needs to hold: engine TCP client, generation map, in-flight task table, build watcher state. These are accessed concurrently from many async tool handlers.

`#[tool_router]` requires `Self: Clone` (the macro stores the service in the router as `Arc<S>`). Standard pattern:

```rust
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    engine: tokio::sync::Mutex<EngineClient>,            // mut ops behind a mutex
    generations: tokio::sync::RwLock<GenerationMap>,     // read-heavy
    in_flight: dashmap::DashMap<TaskId, TaskHandle>,     // concurrent-friendly
    build_state: tokio::sync::watch::Receiver<BuildState>,
}
```

`Arc<inner>` is the standard idiom — rmcp itself stores the service as `Arc<S>` in `Router<S>`, so wrapping our state in another Arc layer keeps `Clone` cheap. Tokio synchronization primitives over `std::sync` because tool handlers are async.

**Cancellation / streaming:**

rmcp **does** expose per-call cancellation. `RequestContext<RoleServer>` carries a public `ct: tokio_util::sync::CancellationToken` field that is cancelled when the agent sends a `notifications/cancelled` for the matching request id (see `handler/server.rs::on_cancelled` and the `RequestContext` source at `service.rs:654-662`). Add `ctx: RequestContext<RoleServer>` as a tool-handler parameter and pass `ctx.ct.clone()` into the supervisor's spawn future, then either `tokio::select!` on `ct.cancelled()` or just hand the token to whatever long-running future is being awaited. This is the right primitive for `run_task` — when an agent aborts, we propagate the cancellation into the engine `kill` RPC.

The token is *not* cloned globally; it's per-request, so cancelling one `run_task` call won't kill others. The `ctx.peer` field is also available if a tool needs to push a notification (e.g. progress) back to the agent during execution.

For tools-list-changed when generations roll over, the supervisor can call `running.peer().send_notification(ToolListChangedNotification::default()).await`, but design says we don't need this (the tool list is stable across generations). Skip.

No streaming surface in MCP for tool *results* themselves — `run_task` stays unary, returning the final report blob, matching design.

**Open questions:**

1. Does `schemars` need to be in our direct deps, or only the rmcp `schemars` feature flag? The macro example uses `#[derive(schemars::JsonSchema)]` against a top-level `schemars` path. rmcp re-exports `pub use schemars` at the crate root (lib.rs), so `rmcp::schemars::JsonSchema` works without a direct `schemars` dep — but the bare-name `#[derive(schemars::JsonSchema)]` form needs a top-level `schemars` crate visible in scope. Likely we need to add `schemars` to `[dependencies]` directly, version-pinned to whatever rmcp's `schemars` feature pulls. **Resolve at impl time** — try without first, add if the macro complains.
2. Custom error codes outside the JSON-RPC reserved range (-32099 to -32000): the spec says these are app-defined; rmcp accepts arbitrary `i32` via `ErrorCode(n)`. Confirm that mainstream clients (Claude Code) don't choke on novel codes — likely fine but worth a quick check during Phase 6.
3. Does `transport-io` (`rmcp::transport::stdio`) silence/redirect logging away from stdout? It must — stdio is the transport — so any tracing logs in the supervisor must go to stderr. Need to confirm and probably configure a `tracing-subscriber` writer pinned to stderr in the `--mcp` entry point. (rmcp-side: the `io` module wraps `tokio::io::stdin()`/`stdout()` directly, so anything else writing to stdout will corrupt the JSON-RPC stream.)
4. The `task_handler` / `enqueue_task` family on `ServerHandler` is for MCP's *task-based* tool invocation (a separate concept from our `TaskDef` — it's the MCP "tool that runs as a background task with its own lifecycle"). For our `run_task`/`spawn_task` design we **don't** want this — `taskSupport` defaults to `Forbidden`, so plain `call_tool` is used. Worth noting because terminology collides badly.

### `t0.r-notify-patterns`

**Existing watcher (`src/watch.rs`).** Full-featured watcher already implemented; exported via `pub mod watch;` in `src/lib.rs:45`. Key facts:

- Uses `notify` v7 (`notify = "7"` at `Cargo.toml:38`) with `RecommendedWatcher` directly — no `notify-debouncer-mini` / `notify-debouncer-full` in `Cargo.toml`.
- Builds a `RecommendedWatcher` per watch with `Config::default()` and `RecursiveMode::Recursive` over a single root directory (`create_notify_watcher`, lines 221–243).
- Bridges notify's blocking callback to tokio via `tokio::sync::mpsc::unbounded_channel`. Closure passed to `RecommendedWatcher::new` calls `event_tx.send(event)` synchronously from notify's worker thread (lines 226–232). No `thread::spawn` dispatcher needed — `mpsc::UnboundedSender::send` is non-blocking and thread-safe.
- A tokio task (`debounce_glob_loop` / `debounce_filter_loop`, lines 307–432) consumes the receiver, accumulates paths from "meaningful" events, uses `tokio::select!` + `tokio::time::sleep_until` for a manual sliding-window debounce (`DEBOUNCE_DURATION` = 100ms at line 167).
- `is_meaningful_event` (lines 438–443) keeps only `EventKind::Create | Modify | Remove`; drops `Access` and `Other`. **Good baseline for the supervisor.**
- Watcher held alive by being moved into the spawned task as `_watcher: RecommendedWatcher` (lines 312, 379). Dropping watcher stops events. Supervisor must keep its watcher alive the same way.
- Each watch creates its own `RecommendedWatcher` rooted at one resolved directory. **No API to add/remove paths from a live watcher**, **no `.gitignore` respect** (uses `globset` only).

**Reusability for the supervisor: NO.** The existing `Watch<T>` API is purpose-built for user tasks (glob → `Vec<PathBuf>` batches, `WatchInfo` for TUI display, `label()` chaining, single-root). The supervisor needs a fundamentally different shape: dynamic watch set across multiple discovered directories, `.gitignore` filtering matching `discover.rs`, no glob, 200ms debounce that produces a "rebuild now" signal not path batches.

What IS reusable:
- The notify→tokio channel bridge pattern.
- The `is_meaningful_event` filter (lift to `pub(crate)` in `watch.rs` or duplicate).
- The `tokio::select!` + `sleep_until` debounce shape.

**Debounce recommendation.** **Manual debounce on top of `RecommendedWatcher`**, mirroring `debounce_glob_loop`. Reasons:
- Codebase already has this pattern working in v7. Adding `notify-debouncer-full` would be a new dep for one consumer.
- `notify-debouncer-full` adds value for rename correlation across events; the supervisor only needs "something changed → rebuild after quiescence."
- Manual pattern lets us trivially set 200ms (vs existing 100ms) and emit unit signal.
- macOS FSEvents already coalesces at the kernel level; library-level dedup is largely redundant.

`DEBOUNCE_DURATION = Duration::from_millis(200)` per design.

**Dynamic watch-set rebuild — `notify` v7 API.** Trait exposes:

```rust
fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<(), Error>;
fn unwatch(&mut self, path: &Path) -> Result<(), Error>;
fn configure(&mut self, option: Config) -> Result<bool, Error>;
```

A single `RecommendedWatcher` instance can have paths added/removed at runtime. Sketch:

```rust
struct SupervisorWatcher {
    watcher: RecommendedWatcher,           // single instance, mutated over time
    watched: HashSet<PathBuf>,             // currently-watched dirs
    rebuild_tx: mpsc::Sender<()>,          // 200ms debounced "rebuild" signal
}

impl SupervisorWatcher {
    fn sync_watch_set(&mut self, desired: &HashSet<PathBuf>) {
        for p in self.watched.difference(desired).cloned().collect::<Vec<_>>() {
            let _ = self.watcher.unwatch(&p);
            self.watched.remove(&p);
        }
        for p in desired.difference(&self.watched) {
            if self.watcher.watch(p, RecursiveMode::NonRecursive).is_ok() {
                self.watched.insert(p.clone());
            }
        }
    }
}
```

Caveats for v7:
- `watch()` returns `Err` if path already watched or doesn't exist; track membership in your own `HashSet` to avoid double-watching.
- On macOS FSEvents, `RecursiveMode::NonRecursive` is honored (FSEvents itself is recursive but notify backend filters). For a RUNME.rs's directory you want `NonRecursive` — `mod foo;` only resolves to siblings; recursive watching would mix in unrelated subtrees that may have their own RUNME.rs (already separately watched).
- After `unwatch()` followed by `watch()` of the same path, expect a brief window where events from the old subscription drain through; debounce naturally absorbs this.

For RUNME.rs files themselves: watching the parent directory non-recursively delivers events for any file in that dir. Don't watch individual files — directory-level watches cheaper and more reliable on FSEvents.

**.gitignore handling.** `discover.rs` already uses `ignore` crate (line 3) with `WalkBuilder::new(...).hidden(true).git_ignore(true).git_global(true).git_exclude(true)`. Supervisor should:
1. Use `ignore::WalkBuilder` for initial RUNME.rs discovery (already done by `discover::discover`).
2. When applying received fs events, re-check ignore rules using `ignore::gitignore::GitignoreBuilder` or by re-running discovery on affected dir. Cheaper: store an `ignore::gitignore::Gitignore` matcher per watched dir (built once) and consult before triggering rebuild.

The `ignore` crate has no callback/event API; gitignore evaluation happens in our event-handling code.

**macOS FSEvents quirks:**

1. **Atomic save = rename-then-create**: vim/Helix/anything using `O_EXCL + rename` produces `Remove(File)` of original then `Create(File)` for new inode. May coalesce into single `Modify(Name(Any))` or appear separate. 200ms debounce folds these into one trigger.
2. **`Modify(Metadata)` floods**: macOS emits metadata events for `mtime`/`atime` changes and Spotlight indexing. Existing `is_meaningful_event` accepts `Modify(_)` broadly — for supervisor consider narrowing to `Modify(Data(_)) | Modify(Name(_))`.
3. **Coalesced event timing**: FSEvents has its own `latency` parameter (notify v7 default ~30ms). Combined with 200ms debounce, ~230ms worst-case rebuild latency.
4. **Rename within watched dir**: comes as `Modify(Name(From))` and `Modify(Name(To))`. Both look like "something changed" — fine for coarse signal.
5. **Volume root / mount**: not a concern for project-local files.
6. **Symlinks**: notify follows them on macOS; `ignore` crate doesn't follow by default. Mismatch could mean we watch under a symlinked dir but discovery doesn't traverse it — unlikely in practice, worth a comment.
7. **Editor backup files** (`.swp`, `~`, `.DS_Store`): trigger events. `.gitignore`/`hidden(true)` filter drops most; explicit filter on extension/filename can guard the rest.

**Tokio bridge pattern (lifted from `src/watch.rs:221-243`):**

```rust
fn create_notify_watcher() -> Result<(RecommendedWatcher, mpsc::UnboundedReceiver<Event>), Error> {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = event_tx.send(event);
            }
        },
        Config::default(),
    )?;
    Ok((watcher, event_rx))
}
```

For supervisor's debounce loop:

```rust
async fn supervisor_event_loop(
    mut event_rx: mpsc::UnboundedReceiver<Event>,
    rebuild_tx: mpsc::Sender<()>,
    mut watcher: RecommendedWatcher,
    /* … desired-set state … */
) {
    const DEBOUNCE: Duration = Duration::from_millis(200);
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        if let Some(dl) = deadline {
            tokio::select! {
                biased;
                maybe = event_rx.recv() => match maybe {
                    Some(ev) if is_meaningful(&ev) && passes_gitignore(&ev) => {
                        deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                    }
                    Some(_) => {}
                    None => return,
                },
                _ = tokio::time::sleep_until(dl) => {
                    let _ = rebuild_tx.send(()).await;
                    deadline = None;
                }
            }
        } else {
            match event_rx.recv().await {
                Some(ev) if is_meaningful(&ev) && passes_gitignore(&ev) => {
                    deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                }
                Some(_) => {}
                None => return,
            }
        }
    }
}
```

Note: differs from `watch.rs`'s loop in two ways: (1) emits `()` rebuild signals not path batches, (2) `watcher` is owned mutably so the supervisor can `sync_watch_set` as RUNME.rs set changes — kept inside this loop with a separate channel for "please add/remove these paths," or `Arc<Mutex<RecommendedWatcher>>` shared with rebuild handler.

**Reuse vs new — recommendation.** **New, supervisor-owned watcher.** Reasons:
- `Watch<T>` is shaped for single-root + glob; supervisor is multi-root + gitignore + dynamic.
- Supervisor needs runtime `unwatch()`/`watch()`; existing module never exposes the watcher post-construction.
- Bolting "dynamic watch set" onto `Watch<T>` would change its public API and add concepts user-facing tasks don't need.
- The notify→tokio→debounce pattern is short enough to copy (~40 lines).

Suggested location: lives next to the supervisor implementation (`src/mcp/build.rs` per the plan). Lift `is_meaningful_event` to `pub(crate)` in `src/watch.rs` so it can be shared, or duplicate with a tighter filter.

### `t0.r-builtin-tasks`

**Registration pattern.** Builtins are defined in `src/builtin.rs` using the standard `#[rnme::task]` macro — same as user-authored tasks. The `mode = cli` attribute pins them to CLI mode regardless of terminal context. Example (`builtin.rs:13-14`):

```rust
const __RNME_GROUP: &str = "builtin";

/// List available tasks
#[rnme::task(mode = cli)]
async fn list(ctx: &TaskContext) -> TaskResult { ... }
```

The macro expands into an `inventory::submit!` call producing a `TaskDef` (`macros/src/lib.rs:488-497`):

```rust
::rnme::inventory::submit! {
    ::rnme::task::TaskDef {
        name: #fn_name_str,
        description: #desc_tokens,
        group: __RNME_GROUP,           // <- read from local const
        func: ::rnme::task::TaskFnKind::Static(#wrapper_name),
        arg_metadata: #arg_metadata_name,
        ui_hint: #ui_hint_tokens,
    }
}
```

Works inside `rnme` crate because `lib.rs` does `extern crate self as rnme`.

**Group assignment (`__RNME_GROUP`).** For RUNME.rs files, codegen injects the const per generated module. For `src/builtin.rs`, declared **manually as a module-local const at the top** (`builtin.rs:10`):

```rust
const __RNME_GROUP: &str = "builtin";
```

The macro expansion references `__RNME_GROUP` by unqualified name. The new `:install-skills` task should be added to `src/builtin.rs` and inherit `__RNME_GROUP = "builtin"` automatically.

**`:` prefix routing.** Lookup happens in `Registry::resolve` at `src/task.rs:1048-1058`:

```rust
pub fn resolve(&self, name: &str) -> Result<&'static TaskDef, TaskError> {
    if let Some(short) = name.strip_prefix(':') {
        return self.tasks.iter()
            .find(|t| t.name == short && t.group == "builtin")
            .copied()
            .ok_or_else(|| TaskError::from_display(format!("unknown built-in task: {}", short)));
    }
    // ... group:task qualified, then short-name with root-wins
}
```

So `:install-skills` strictly matches `name == "install-skills" && group == "builtin"`. Resolution invoked from CLI dispatch at `src/cli.rs:118-127` (`registry.resolve(task_name)`) and from `TaskContext::run` at `src/task.rs:803`. Unit test for colon prefix routing at `src/task.rs:1454-1470`.

**⚠ Constraint flagged for synth/G0:** Hyphens in task names are valid in lookup, but **Rust function names can't contain hyphens**. The function must be named `install_skills` (underscore), so the registered name will be `install_skills`, not `install-skills` (`name: #fn_name_str` is hard-wired in the macro at `macros/src/lib.rs:490`).

The design doc says `:install-skills` but the codebase produces `:install_skills` from `fn install_skills`. **Decision needed** in implementation. Options:
- Use `:install_skills` to avoid macro changes (recommended, simplest).
- Treat hyphen↔underscore as equivalent in `Registry::resolve`.
- Add a `name = "install-skills"` override to the `#[rnme::task]` attribute syntax.

**Argument pattern.** The macro supports three forms (`macros/src/lib.rs:19-30`, doc lines 178-212):

1. **ZeroArgs** — `async fn task(ctx: &TaskContext) -> TaskResult`
2. **SimpleArgs** — multiple primitive params become `--flag` args (NOT positionals). E.g. `fn deploy(ctx: &TaskContext, env: String, port: u16)` exposes `--env` and `--port`. See arg builder at `macros/src/lib.rs:526-532`: `Arg::new(...).long(long_name).required(true).action(ArgAction::Set)`.
3. **ParserStruct** — single non-primitive param implementing `clap::Parser`. The macro calls `<Type as ::rnme::clap::Parser>::try_parse_from(...)` (`macros/src/lib.rs:410-418`).

For a positional `<target>` argument, **Form 3 (Parser struct) is required** because Form 2 only emits `--long` flags. Sketch:

```rust
#[derive(clap::Parser)]
struct InstallSkillsArgs {
    /// Target directory (rnme/<skill>/ subtree will be created under here)
    target: std::path::PathBuf,
}

/// Install bundled skills (rnme/<skill>/) into <target>.
#[rnme::task(mode = cli)]
async fn install_skills(ctx: &TaskContext, args: InstallSkillsArgs) -> TaskResult {
    let report = rnme::skills::install_to(&args.target).await?;
    ctx.println(format!("installed {} skills into {}", report.count, report.target.display())).await;
    Ok(())
}
```

The shared `install_to(path)` library function is what the MCP tool `install_skills(target_dir)` also calls.

**Path-arg precedent.** **No existing builtin takes a filesystem path arg.** All four current builtins (`list`, `fmt`, `check`, `clean`) are zero-arg. Closest path-handling helpers are private:
- `cache_dir()` (`builtin.rs:137-141`) — pulls `RNME_CACHE_DIR` env var.
- `runme_files()` (`builtin.rs:143-151`) — splits `RNME_RUNME_FILES` env var.

Neither reusable for `<target>` arg. Implementation will canonicalize / create the directory (recursive `mkdir`, then `canonicalize`) per design contract.

`clap::Parser` derive isn't currently used anywhere in `src/builtin.rs`, but is fully supported by the macro pipeline. `clap` is re-exported at `src/lib.rs:81` so `#[derive(clap::Parser)]` works directly.

**Async requirement.** Yes — `#[rnme::task]` produces a `TaskFn` of type `for<'a> fn(&'a TaskContext, &[String]) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>>` (`task.rs:63-66`). Macro supports both `async fn` and sync `fn` (4 wrapper variants at `macros/src/lib.rs:437-479`). For consistency with other builtins, declare `async fn install_skills(...)`. Sync IO inside async is fine for a one-shot installer; if strict, wrap with `tokio::task::spawn_blocking`.

**Other relevant context:**

- Shared `Registry` built from inventory via `Registry::from_inventory()` (`task.rs:1002-1008`). Adding a new `#[rnme::task(mode = cli)]` to `src/builtin.rs` is automatically picked up.
- The argument metadata function (`arg_metadata: ArgMetadataFn`, `task.rs:109`) is used by the TUI picker to render `--help` (`tui/picker.rs:467`). Form 3 produces `Some(<Type as CommandFactory>::command())` (`macros/src/lib.rs:421-424`), so `--help` for `:install-skills target` works.
- CLI dispatch passes `args.rest[1..]` straight through as `Vec<String>` to the task wrapper (`cli.rs:119, 173, 228`). Wrapper invokes clap; bad args produce `TaskError`.

### Context brief (synth)

Phase 0 research is complete. The six researcher entries above are detailed and citation-rich; this brief does not paraphrase them. Use them as the primary source when each implementor lands. This brief surfaces only the cross-cutting decisions, ordering nuances, plan adjustments, and risks that the human needs at G0.

#### Decisions for G0

These are real holes — questions the design doc didn't (or couldn't) answer, raised by the research. Each has a recommended default; the human can say "yes, all defaults" and Phase 1 starts.

1. **`:install-skills` vs `:install_skills` (task name).** Design doc consistently writes `:install-skills` (hyphen). The `#[rnme::task]` macro hard-codes `name: #fn_name_str` (`macros/src/lib.rs:490`), and Rust function names cannot contain hyphens. The function will be `install_skills`, so the registered task name will be `install_skills`. Three options surfaced by `r-builtin-tasks`:
   - (a) ship as `:install_skills` (no macro change, smallest surface);
   - (b) treat `-` and `_` as equivalent in `Registry::resolve` (impacts every builtin and user task — quiet semantic change);
   - (c) extend `#[rnme::task]` with `name = "install-skills"` override (precedent set, then user tasks can also override; bigger scope).
   - **Recommended default: (a) `:install_skills`.** Update the design doc reference to match. Rationale: the design doc's hyphen is a stylistic choice from prose; the macro's identity-naming rule is the codebase's actual contract. (a) is the only option that does not introduce a new code path.

2. **Outer-driver path for supervisor re-invocation.** Design §"Topology" says "spawn next `rnme --engine` (which compiles itself)". `r-cli-dispatch` Subtlety #2 is the gotcha: when the user runs `rnme --mcp`, the outer `main.rs` already `execvp`s into the compiled runner (cache-dir binary). After exec, `current_exe()` resolves to the runner, not the outer driver — so a naive `Command::new(current_exe())` re-spawns an already-compiled artifact and never picks up edits.
   - **Recommended default: env-var route.** Outer `src/bin/rnme/main.rs` already sets `RNME_CACHE_DIR` and `RNME_RUNME_FILES` before `execvp`; add `RNME_DRIVER_EXE = std::env::current_exe()?` next to those (one-line change). Supervisor reads it and does `Command::new(env::var("RNME_DRIVER_EXE")?).arg("--engine")…`. This is a precise change that belongs in `i-supervisor-core`'s plan-approval step.
   - Note: this is the *only* mechanism by which generation N+1 picks up edited code, so getting it right is load-bearing for Phase 5's edit-during-task scenario. If skipped, every gen runs the same compiled artifact and the watcher does nothing visible.

3. **rmcp Cargo.toml feature additions.** Today `Cargo.toml` has `rmcp = "1.6.0"` (default features only — `server` + `macros`). `r-rmcp-api` shows we need `transport-io` (for `rmcp::transport::stdio`) and `schemars` (for the `JsonSchema` derives on tool param types). May also need `schemars` as a direct dep depending on macro path resolution.
   - **Recommended default:** when `i-supervisor-core` or `i-mcp-tools` first touches `Cargo.toml`, change to `rmcp = { version = "1.6.0", features = ["server", "macros", "transport-io", "schemars"] }`. Add a direct `schemars` dep only if the macro errors without it; remove if not needed.

4. **stdout discipline under `--mcp` (logging redirect).** rmcp stdio transport owns stdout. Any `tracing` / `println!` / `eprintln!` to stdout corrupts the JSON-RPC stream. `r-rmcp-api` open question #3 flags this.
   - **Recommended default:** in the `--mcp` arm (before `Supervisor::serve`), install a `tracing-subscriber` that writes to stderr only, and audit the supervisor + build modules for stray `println!`. Belongs in `i-supervisor-core`'s implementation, not a separate task.

5. **`--engine` / `--mcp` help-text visibility.** `r-cli-dispatch` open question #3.
   - **Recommended default:** visible (no `hide = true`). They're sibling entry points per design and discoverable surface helps users debug agents. Re-evaluate if the help block becomes cluttered.

6. **`--gen-cooldown` flag.** `r-cli-dispatch` flagged this is missing from the `RnmeArgs` sketch. Design §"Generations" specifies it (default 15min).
   - **Recommended default:** add `--gen-cooldown <duration>` with `requires = "mcp"` when `i-supervisor-core` lands. Parse via humantime or seconds-as-u64; default 900.

7. **`mark_terminal()` consolidation in `i-task-execution`.** `r-engine-internals` finding #1 plus the explicit refactor suggestion: there are *three* terminal-status mutator sites, not one (body completion at `execution.rs:382-399`, cancel at `engine.rs:428-434`, timeout at `engine.rs:499-504`). All three need `ended_at` wiring.
   - **Recommended default:** consolidate behind `EngineInternals::mark_terminal(&self, exec, status)` (or an equivalent on `TaskExecution`) that writes status + `ended_at` together and calls `publish_snapshot`. This is an implementation choice for `i-task-execution` to propose at plan-approval time, but the human should know it's the recommended path because three-way drift on terminal-state writes is a class of bug we don't want.

8. **`ingest_buffer()` re-stamping in `i-logstore`.** `r-logstore` flagged that today `LogStore::ingest_buffer` forwards entries with whatever seq the buffer carries. After migration, buffer entries no longer have meaningful seq.
   - **Recommended default:** re-stamp via `push()` so every persisted entry's seq is engine-global. Belongs in `i-logstore`'s implementation; calling it out so the implementor doesn't preserve the old "trust upstream seq" path.

#### Cross-phase coupling

Places where Phase N's work has non-obvious consequences for Phase M.

- **Phase 1 `i-logstore` breaks 3-4 specific tests.** From `r-logstore`: `src/process.rs:1501-1503` (`test_log_entry_source_and_seq`), `src/tracing_layer.rs:240` (`test_tracing_layer_captures_info_with_fields`), `src/tracing_layer.rs:270-273` (`test_tracing_layer_multiple_levels`), and `src/log/store.rs:429` (`test_compose_same_seq_deterministic`). The implementor should be told exactly where; the validator (`v-logstore`) should not be surprised when these fail. The first three need updating to either route through `LogStore::push` or assert strict-monotonicity rather than equality. The last is testing a property that is **impossible** under the new scheme (same seq from different sources) and should be removed or repurposed, not "fixed".

- **Phase 1 `i-task-execution` snapshot field copy site.** All three new fields (`started_at`, `ended_at`, `summary`) flow through one place: `EngineInternals::publish_snapshot` at `src/execution/engine.rs:267-278`. `r-engine-internals` finding #5 is precise about the locking pattern (plain field for `started_at`; `Mutex` lock-and-clone for the other two). `i-task-execution` and `i-wire` (Phase 2) both consume this — `i-wire` needs `Serialize` derives on the new `TaskNode` fields.

- **Phase 1 `i-task-info` belongs with the engine, not the wire.** Design §"Single source of truth" was explicit; `r-cli-dispatch` confirms registry+group_names already flow into `cli::run` at the runner-binary level. `TaskInfo` lives in `src/task.rs`, not `src/mcp/wire.rs` — `i-wire` re-exports / derives serde on it but does not redefine it. Worth restating to the `i-wire` implementor at plan-approval.

- **Phase 2 `i-wire` serde derive scope.** From the design and `r-engine-internals` + `r-logstore`: derives needed on `GraphSnapshot`, `TaskNode` (including new `started_at`/`ended_at`/`summary`), `ProcessNodeInfo`, `TaskStatus`, `ProcessStatus`, `TaskId`, `KillSignal`, `SpawnOptions`. `LogEntry` already has `Serialize`; needs `Deserialize` audited. `chrono::DateTime<Local>` serde needs the `serde` feature on `chrono` if not already enabled — confirm in `i-wire`'s plan-approval.

- **Phase 4/5 supervisor re-invocation depends on Decision #2 above.** If the human picks a non-default for #2 (e.g. accepts `current_exe()` and design doc updates to "no recompile per gen"), Phase 5's file-watcher loses its primary purpose and the design needs a separate edit. **Strongly recommend the env-var default.**

- **Phase 6 `i-mcp-tools` Cargo.toml change.** Per Decision #3, this is where rmcp features get added. If the work is sequenced so `i-supervisor-core` lands first and needs rmcp types compiled, that implementor adds the features instead. Either way the change is one PR-line and not a blocker.

- **Phase 7 `:install_skills` builtin uses Form-3 args (clap Parser).** From `r-builtin-tasks`: there is **no existing builtin with a positional path arg**. `i-skills` is the first user of `clap::Parser` derive in `src/builtin.rs`. The pattern works (the macro pipeline supports it) but this is an unblazed trail — flagging so the implementor doesn't assume there's a precedent to follow.

- **Existing `src/watch.rs` is NOT reused.** `r-notify-patterns` is firm: supervisor needs a different shape (multi-root, gitignore-aware, dynamic, unit-signal output). The only reuse is `is_meaningful_event` (lift to `pub(crate)` or duplicate) and the notify→tokio bridge pattern. `i-build-state` should not get pulled into refactoring `Watch<T>`.

#### Plan adjustments

Small mechanical changes worth making to the plan based on Phase 0:

1. **Add a one-line task in Phase 4 or Phase 5 for the `RNME_DRIVER_EXE` env-var addition in `src/bin/rnme/main.rs`.** Currently nothing in the plan owns this change; it's a precondition for `i-supervisor-core`'s `spawn_engine`. Easiest: roll into `i-supervisor-core`'s scope explicitly.

2. **Add `--gen-cooldown` to `i-supervisor-core`'s scope.** Currently absent from both the cli-dispatch sketch and the supervisor-core task description. One sentence in the task's "behavior" list.

3. **Phase 1's `i-task-execution` plan-approval step should explicitly call out the `mark_terminal()` consolidation question.** Mentioned in `r-engine-internals` recommendation, not currently in the task description.

4. **Phase 2's `i-wire` plan-approval step should list the exact set of types needing serde derives.** Avoids a back-and-forth in implementation. The list is in this brief's "cross-phase coupling" item above.

5. **Phase 1 `v-logstore` task description should pre-list the failing tests.** So the validator knows the expected diff, not "tests broke, file a bug." Current description is generic; specific filenames/line numbers come from `r-logstore`.

6. **No phases need to be renamed, removed, or reordered.** Phase 0 confirmed the eight-phase shape is sound. Phase 7 (skills) can start immediately after G0 in parallel with Phases 1–6 as the plan already says.

#### Risks

Top 5 by impact, most concerning first.

1. **Outer-driver re-invocation (Decision #2).** If implemented naively with `current_exe()`, the supervisor will spawn child engines from the cache-dir runner and edits will never reach a new generation. Symptom: the file watcher fires, a new gen spawns, but tasks behave as though no edit happened. Diagnostic friction is high because everything else looks correct. *Mitigation:* env-var default + a Phase 5 integration test that asserts a specific edited line shows up in gen 2's task output.

2. **`mark_terminal()` drift.** Three sites already exist for terminal-status writes. Adding `ended_at` triples the chance one path forgets to write it. Symptom: `get_task` reports look correct most of the time but `Run time` is missing or wrong on cancelled/timeout paths intermittently. *Mitigation:* consolidate behind one helper in `i-task-execution` (Decision #7).

3. **stdout corruption under `--mcp` (Decision #4).** Any stray `println!` / `tracing` writer pointed at stdout breaks the JSON-RPC stream. Symptom: agent gets parse errors and the server appears broken even though tools work. Hard to debug because the corruption can be intermittent. *Mitigation:* dedicated stderr-only subscriber in `i-supervisor-core`; lint sweep for `println!` in `src/mcp/`.

4. **`OutputBuffer` push paths bypass `LogStore::push`.** From `r-logstore`: tracing layer pushes into a bare `OutputBuffer`, no LogStore stamping. Under the new scheme its entries get `seq: 0`. Symptom: tracing-emitted log lines appear out of order in TUI/MCP cursor paging. *Mitigation:* `i-logstore` plan-approval should explicitly check whether the tracing layer needs to flow through `LogStore::push` or accept a "seq stamped on first ingest" path. This may be a quiet design hole worth raising at G1.

5. **macOS FSEvents quirks during atomic-save edits.** From `r-notify-patterns`: vim/Helix produce `Remove` + `Create`, not `Modify`. The 200ms debounce should fold these but if `is_meaningful_event` is too narrow (e.g. drops `Remove`), edits go undetected. *Mitigation:* `i-build-state` reuses `is_meaningful_event` semantics and `v-build-state` includes a vim-style atomic-save scenario in its test list (currently the validator description says "file edit during running task" without specifying the editor pattern).

#### Recommended G0 default

If the human says "yes, all defaults, go", Phase 1 starts with:

- Decision 1 → `:install_skills` (underscore). Update design doc reference at impl time.
- Decision 2 → Add `RNME_DRIVER_EXE` env var in outer `src/bin/rnme/main.rs`. Supervisor uses it.
- Decision 3 → Cargo.toml: `rmcp = { version = "1.6.0", features = ["server", "macros", "transport-io", "schemars"] }`. Add direct `schemars` dep only if needed.
- Decision 4 → Stderr-only `tracing-subscriber` installed in `--mcp` arm. Lint sweep for stdout writers in `src/mcp/`.
- Decision 5 → `--engine` and `--mcp` visible in `--help`.
- Decision 6 → `--gen-cooldown <duration>` flag added to `RnmeArgs` (default 900s, `requires = "mcp"`).
- Decision 7 → `i-task-execution` consolidates terminal-state writes behind one helper.
- Decision 8 → `i-logstore::ingest_buffer` re-stamps via `push()`.

Plus the plan adjustments listed above (RNME_DRIVER_EXE in `i-supervisor-core` scope, `--gen-cooldown` in same, `mark_terminal()` callout in `i-task-execution`, serde-derive list in `i-wire`, failing-test list in `v-logstore`).

Override any of the above before Phase 1 starts; otherwise we go.

---

## Decisions Log

**G0 (2026-05-07) — Phase 0 review.** All 8 synth-surfaced decisions resolved:

1. **Builtin task name** → `:install_skills` (underscore). Codebase contract wins; design doc reference is stale prose.
2. **Supervisor placement** → outer `src/bin/rnme/main.rs` short-circuits on `--mcp` before compile. No exec, no env-var hack. `current_exe()` from supervisor returns outer rnme, which handles its own re-invocation transparently.
3. **rmcp Cargo.toml features** → `features = ["server", "macros", "transport-io", "schemars"]`.
4. **stdout discipline under `--mcp`** → install stderr-only `tracing-subscriber` in supervisor entry; lint-sweep `src/mcp/` for stdout writers.
5. **`--engine` / `--mcp` help-text** → visible (no `hide = true`).
6. **Generation cooldown** → **removed entirely.** Old gens with completed tasks stay alive for the supervisor's lifetime. Only gens that never had a task retire mid-session (immediately, when eclipsed by another rebuild). No `--gen-cooldown` flag. Design doc has stale text — flagged at top of plan.
7. **`ended_at` writer sites** → write at all three existing terminal-status writers (body completion, cancel-after-abort, timeout-after-abort). No consolidation requirement; helper extraction is implementor's tactical choice.
8. **`LogStore::ingest_buffer` under engine-global seq** → re-stamp via `push()` so every persisted entry has an engine-global seq.

---

## Blockers

*(Populated during execution.)*

---

## Notes for the executor

- **Plan-approval implementors** (`yes` in the team table) propose their approach to `mcp-lead` before writing code. The proposal should: name the files they'll touch, describe the public types/functions they'll add, and call out any deviation from the design doc with reasoning.
- **Do not relitigate design decisions.** The design doc has already been through extensive iteration. If an implementor finds a hole, surface it through `mcp-lead` to the human — don't decide unilaterally.
- **Engine type discipline.** Per design §"Single source of truth", the wire layer must not introduce parallel copies of engine types. If a serde derive is missing, add it on the engine type, not on a wire-side wrapper.
- **No backwards compatibility shims.** This is unreleased software with one user (the project owner). Breaking changes are fine. Don't carry forward old field names "just in case."
- **Slice 1 buildability.** Per the user's iterative-development memory, intermediate slices don't need to compile cleanly between commits, but each phase boundary should produce a working binary. Validators run at phase boundaries.
