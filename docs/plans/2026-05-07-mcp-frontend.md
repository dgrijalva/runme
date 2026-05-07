# MCP Frontend Implementation

**Status:** draft
**Source design:** `docs/mcp_design.md`
**Created:** 2026-05-07

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
- [ ] `t1.i-task-execution` (depends on `t0.synth`) — Add `summary: Mutex<Option<String>>`, `started_at: chrono::DateTime<Local>`, `ended_at: Mutex<Option<DateTime<Local>>>` to `TaskExecution`. Wire `started_at` at body-spawn; wire `ended_at` at terminal-status mutator (write status + `ended_at` together). Add `TaskContext::summary(impl Into<String>)`; add `TaskNode.summary: Option<String>` to the snapshot. `summary` writes publish a fresh snapshot.
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
- [ ] `t4.i-supervisor-core` (depends on G3) — Create `src/mcp/supervisor.rs` and add `--mcp` arm to `src/cli.rs`:
  - `Supervisor` state: `Vec<Generation>` (each holding a `tokio::process::Child`, a TCP write half, a snapshot watcher, an in-flight `HashMap<CorrelationId, oneshot::Sender>`, the latest cached `GraphSnapshot`, the open subscriptions); a `latest_gen: GenerationId` pointer
  - Per-gen connection task: read loop that demultiplexes `WireMessage::Response` (via correlation map), `WireMessage::Event::Graph` (updates cache, fires watch update), `WireMessage::Event::Log` (forwards to subscribers)
  - `spawn_engine(start_task_id)`: `tokio::process::Command::new(current_exe).arg("--engine").arg("--start-task-id").arg(N).stdout(piped).stderr(piped).spawn()` — read first stdout line for port, connect TCP, register as gen
  - Generation lifecycle: promote-latest on port-line, retire-immediately when never had tasks, cooldown timer (default 15min, sliding TTL on access) when had tasks but is no longer latest
  - Cleanup: on supervisor drop, close all TCP connections (engines clean themselves up via EOF path)

  This task is the **highest-novelty slice** — propose plan, lead approves, then implement.
- [ ] `t4.v-supervisor` (depends on `t4.i-routing` + `t4.i-supervisor-core`) — Smoke test: launch supervisor with a single RUNME.rs containing one task; confirm gen 1 spawns; send `ListTasks` through supervisor → forwarded to gen 1 → response returned; send `SpawnTask` → confirm dotted address comes back; close connection → confirm engine cleans up.

**Gate G4:** Human reviews supervisor + routing.

---

### Phase 5 — File watcher + build state machine

**Tasks:**

- [ ] `t5.i-build-state` (depends on G4) — Create `src/mcp/build.rs`:
  - File watcher with `notify::RecommendedWatcher`, 200ms debounce
  - Watch set: every discovered RUNME.rs + every sibling `.rs` file in each RUNME.rs's directory, respecting `.gitignore`. Rebuild watch set when discovery changes.
  - **Note:** the supervisor's discovery pass exists *only* to populate the watch set. The child engine does its own discovery internally during compile. Do not duplicate compile logic in the supervisor.
  - `BuildState` enum: `Idle`, `Rebuilding`, `LastBuildFailed { last_failure_output: String }`. Single-state, separate from per-engine lifecycle.
  - State transitions per design §"Build state machine"
  - Tool routing: spawn-shaped vs existing-state per design table
  - On debounced event: spawn next gen via `Supervisor::spawn_engine(past_last_used_id)`; route new spawns there on success; retire old gen-with-no-tasks immediately on success
  - Capture child stderr into `last_failure_output` if child exits before printing port line; surface its head (~12 lines) on `spawn_task` / `run_task` errors
- [ ] `t5.v-build-state` (depends on `t5.i-build-state`) — Tests:
  - File edit during running task → new gen spawns, old task keeps running, new spawns route to new gen
  - Edit RUNME.rs to make compile fail → next `spawn_task` returns build error with cargo head; `get_build_status` returns full output; existing `get_logs` against running task still works
  - Cooldown: spawn task on gen 1, retire (latest is now gen 2), task completes — gen 1 enters cooldown — `get_logs` against its tasks resets timer — eventually expires, returns `not_found`
  - `kill_all` → all non-latest engines retire; latest survives with no tasks

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
*(pending)*

### `t0.r-logstore`
*(pending)*

### `t0.r-cli-dispatch`
*(pending)*

### `t0.r-rmcp-api`
*(pending)*

### `t0.r-notify-patterns`
*(pending)*

### `t0.r-builtin-tasks`
*(pending)*

### Context brief (synth)
*(pending)*

---

## Decisions Log

*(Populated during execution.)*

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
