# MCP Frontend Design

The MCP (Model Context Protocol) frontend exposes rnme's task runtime to coding agents. This document captures the architecture decisions made before implementation; for the underlying engine, see `runtime_engine_design.md`.

## Goal

Give an agent a stable interface for driving rnme over the lifetime of a session: list tasks, run them (foreground or backgrounded), monitor their state, query their output. The interface needs to survive RUNME.rs file edits during the session — agents can't be told "restart your MCP server" mid-conversation, and many MCP clients disable servers that crash or exit unexpectedly.

A second use case lands for free once that surface exists: tasks are structurally tool-like, and an agent can _author_ them by editing RUNME.rs directly. The same loop that absorbs user edits — file watch, rebuild, fail spawns with the cargo error when builds break, route new spawns to the new build when they succeed — is the authoring loop. Operating user-written tasks and writing new ones are the same flow from the agent's perspective. Tasks become a way for an agent to extend its toolbox with reviewable, version-controlled code that humans can read and run too, rather than per-session tool definitions. The skills library (see Skills) is what teaches an agent the RUNME.rs idioms it needs to be productive in the authoring direction; the runtime architecture below already supports both flows without modification.

## Topology

```
agent ─stdio─▶ rnme --mcp ─┬─TCP/JSONL─▶ rnme --engine  (gen 1, kept alive — logs queryable)
                  │         ├─TCP/JSONL─▶ rnme --engine  (gen 2, latest — receives new spawns)
                  │         └─TCP/JSONL─▶ rnme --engine  (gen N, spawned on next rebuild)
                  │
                  │  on RUNME.rs change: spawn next `rnme --engine` (which compiles
                  │                      itself); on success route new spawns there;
                  │                      old gens stay alive for the supervisor's lifetime
```

Two new mode flags. They live at different layers because their needs are different:

- **`--engine`** — engine + TCP JSONL server bound to `127.0.0.1:0` (OS-assigned port). Headless. No TUI, no stdio forwarding. The "engine daemon" mode. On startup, prints `{"port": <u16>}` as a single line on stdout (the _only_ thing it ever writes to stdout); errors go to stderr. Accepts `--start-task-id N` to set the starting `TaskId` counter; defaults to 1 (the standalone case). The supervisor passes a starting id past anything previously used so live engines occupy disjoint id ranges and top-task ids stay globally unique.

  Dispatched in the **runner binary** (`src/cli.rs`) like `--tui` / `--cli`, because the engine needs the registry baked in via `inventory` from the user's compiled RUNME.rs files.

- **`--mcp`** — MCP server on stdio (parent agent), implemented with `rmcp`. Maintains a map from top-task id to engine (and the underlying `rnme --engine` children). Parses dotted addresses on incoming MCP tool calls (see Identifiers and routing), looks up the engine that owns the address's `<top>`, forwards the request to that engine's TCP connection. Watches RUNME.rs files; on change, spawns a new engine and routes new top-level spawns there. Child stderr is dropped (or forwarded later if useful).

  Dispatched in the **outer driver** (`src/bin/rnme/main.rs`), short-circuited *before* `compile_workspace()`. The supervisor runs in the outer process — no discover, no compile, no exec into a runner. The supervisor has no need for the user's task code; it's a pure proxy/router. From inside the supervisor, `std::env::current_exe()` returns the outer `rnme` binary, so `Command::new(current_exe()).arg("--engine")` re-enters the outer driver, which transparently does discover+compile+exec into a runner with `--engine` (the engine daemon).

Like `--tui` and `--cli`, these are bare flags; passing more than one mode flag is rejected by the arg parser.

## Why this shape

The shape was reached by removing complexity from a more ambitious starting point:

1. **One RPC layer, not two.** An earlier draft separated a "supervisor↔engine internal protocol" from a "public RPC for external consumers." There are no external consumers today — only MCP. A single internal-but-honest RPC, versioned with the rest of the crate, is enough. If a web UI or remote agent shows up later, that's the right time to think about stability and auth — not now.
2. **No loopback abstraction.** macOS/Linux only. TCP `127.0.0.1` is fine; no Unix sockets, no Windows.
3. **No streaming subscriptions over HTTP/SSE.** A single persistent TCP connection per gen carries newline-delimited JSON in both directions — request/response _and_ server-pushed events ride the same machinery. Avoids long-polling, avoids a separate streaming endpoint shape, avoids gRPC weight, and avoids WebSocket's framing/handshake/ping overhead since none of it earns its keep on a loopback hop between two processes built from the same crate.
4. **TUI stays in-process.** Tempting to make the TUI a client of the RPC for symmetry. Don't. The TUI's tight coupling to `watch::Receiver<GraphSnapshot>` and `broadcast::Receiver<LogEntry>` is a feature, not an accident. `rnme` (interactive) and `rnme --mcp` (headless+agent) are sibling entry points, not stacked.
5. **Engine-as-child for MCP, not in-process.** Restarting an MCP server without restarting the agent is unreliable in practice — many clients disable MCPs that exit. Keeping MCP alive across rebuilds means the engine has to be a separate process the supervisor can manage. With generational supervision (see Generations), engine-as-child also lets long-running tasks survive unrelated edits — old generations stay alive until their tasks drain naturally, while new spawns route to the rebuilt latest gen. The cost is one extra process per live gen + the TCP connection; the benefit is the agent never sees a disconnect, and editing one task doesn't kill an unrelated running service.

## Generations

The supervisor maintains a list of engine children — generations — rather than kicking the single child between rebuild cycles. Each generation runs on a specific build of the user's RUNME.rs files; tasks spawned in gen N execute the code that was current when gen N was built.

### Why

A running task uses the code it was built against. If `npm run dev` is running on gen 1's build, an unrelated edit shouldn't kill it. New spawns after the edit get gen 2's build with the fresh code; gen 1's `npm run dev` keeps going on its old code until something stops it. The agent's existing task IDs stay live, and long-running services survive editing — which is the actual user pain point.

The cost is contained to the supervisor: engine code is unchanged, and the agent doesn't see generations as a concept.

### Lifecycle

```
file event → debounce 200ms → spawn `rnme --engine` (gen N+1)
  child runs discover/compile internally
  on port-line: gen N+1 becomes "latest"; new spawns route there
  on early-exit: keep current latest gen; surface stderr (cargo errors) on next
                 spawn / list_tasks / run_task call

per-gen retirement:
  - latest gen is never retired.
  - a gen with running tasks: never retired.
  - a gen whose tasks are all terminal AND is not latest: stays alive for the
    supervisor's lifetime. The agent can keep querying its logs and reports for
    as long as the MCP session is open.
  - a gen that never had tasks (spawned, then immediately eclipsed by another
    rebuild before any task spawned through it): retire immediately — no logs
    of value, no reason to keep the process around.
```

**The MCP session is the retention boundary, not a timer.** Old gens whose tasks have completed sit in memory with their `LogStore` intact; the agent can come back to a completed task five minutes or five hours later and still get its logs and rendered report. This is the user pain point the generations system exists to solve — losing data because of an unrelated edit is the failure mode we're avoiding, and a timer-based eviction recreates a softer version of that failure. The cost is one process and one TCP connection per edit cycle, bounded by how many times the user edits during a single MCP session, which in practice is small enough not to matter.

When the supervisor exits (MCP session ends), all gens shut down via the EOF-cleanup path — see "Engine cleanup on disconnect" below.

### Identifiers and routing

Addresses are dotted strings: `<top>.<task>.<seq>`.

- `<top>` — the top-level task id, the user-spawned ancestor. Routes the supervisor to the engine that owns this subtree.
- `<task>` — the actual `TaskId` (engine-internal). For a top-level task, `<task> == <top>`; we accept either `42` or `42.42` for the same target.
- `<seq>` — log sequence number when applicable (cursor or specific entry reference). Engine-global seq from `LogStore::next_seq`.

Top-level task ids are globally unique across the supervisor's lifetime _by construction_: when the supervisor spawns a new engine, it passes `--start-task-id N` past anything previously used. Engines allocate task ids monotonically from N. Live engines occupy disjoint id ranges, so top-task ids never collide. Sub-task ids (tasks/processes spawned by a top-level task) come from the same engine counter and are addressed as `<top>.<sub>`.

Strings over packed integers because they read cleanly in agent-facing tools — `t42.7` is meaningfully better than `t100000000000007` when an agent is composing follow-up calls. The supervisor parses on inbound, formats on outbound; engine-internal types are unchanged.

- **Tool calls that take an id** (`kill_task`, `get_task`, `get_logs`, `kill_process`, etc.) — supervisor parses the dotted form, looks up the engine that owns `<top>` in its top-task→engine map, forwards `task_id: <task>` (and `seq: <seq>` if any) on that engine's TCP connection.
- **Snapshots and log entries flowing from an engine back to the supervisor** — supervisor walks each engine's snapshot, identifies the top-level ancestor of every node, formats embedded ids as `"<top>.<task>"` before passing them to the agent.
- **Graph view** — `get_graph` returns a flat list of live top-level tasks (with their subtrees) merged across all engines, ordered by top-task id ascending. No supervisor-level meta-root, no reserved id.
- **Stale ids** — an id whose `<top>` references a never-had-tasks gen that retired (or any other supervisor-side miss) returns `not_found` without crossing the TCP boundary. Same for log cursors. In practice this is rare during a session: gens with tasks stay alive for the session's lifetime, so the only `not_found` paths are malformed/unknown ids and ids from gens that retired immediately for having no tasks.
- **Malformed ids** — anything that doesn't parse as `<u64>(\.<u64>(\.<u64>)?)?` returns `bad_request`.

This means **no engine code change** beyond what's needed for the TCP/JSONL transport itself. Engine-ownership tracking, snapshot merging, and address parsing all live entirely in the supervisor.

### Retention model

State on a gen survives as long as the gen does, and gens with tasks live for the supervisor's lifetime. So in practical terms: **once a task has run during an MCP session, its logs and rendered report stay queryable for that whole session.** This matches the in-process "completed tasks stay forever" property of the standalone engine — "forever" is just bounded by the session.

The supervisor exits → all gens exit via EOF-cleanup → all `LogStore` / graph state is dropped. Cross-session retention is out of scope for v1; an agent that needs durable history of a task should capture `get_task` output (the rendered report) into its own notes during the session.

A future supervisor feature could persist gen state to disk on shutdown for cross-session retention. Not v1.

### Engine cleanup on disconnect

The engine's lifeline is its TCP connection to the supervisor. On read EOF or write error, the engine assumes the supervisor is gone — crashed, exited cleanly, or explicitly closed the connection to retire this gen — and shuts itself down:

1. Cancel all tasks via the existing root-cancel path (which already fans out to the cancel ladder per task).
2. Wait up to `kill_timeout` for clean exits.
3. SIGKILL any survivors — process groups make this clean.
4. Drop `LogStore` / graph state.
5. `exit(0)` on clean shutdown, `exit(1)` if any children resisted SIGKILL.

The consequence: **the supervisor's "retire this gen" mechanism is just "close the TCP connection."** No explicit shutdown RPC, no two-phase handshake. Engine sees EOF, runs cleanup, exits. Supervisor reaps via its existing `tokio::process::Child` handle and drops gen state. macOS has no `PR_SET_PDEATHSIG` equivalent, so we don't have an OS-level "parent died" signal — the TCP connection is our parent-watch.

There is no reconnect: even if the connection drops for a transient supervisor freeze (vanishingly unlikely on loopback), the engine tears down. The broader stance — "if the loopback connection drops, something is very wrong" — applies.

## RPC protocol

Loopback TCP between the supervisor and one engine child per gen, framed as JSONL: one JSON message per line, `\n`-terminated, `serde_json`-encoded compactly (no embedded newlines from `to_string_pretty`). Both ends are the same crate at the same version, so the protocol is just a shared Rust enum serialized through a single `send`/`recv` helper. No JSON shape spec, no semver, no auth — change the enum, both sides recompile together.

**IDs on the wire are raw engine types** (`TaskId`, the global `LogStore` seq). The dotted-address scheme (see Identifiers and routing) is purely an MCP-boundary concern; the engine has no awareness of generations or top-task→engine routing.

### Single source of truth: engine types ARE wire types

The wire protocol uses the engine's existing snapshot/value types directly. `GraphSnapshot`, `TaskNode`, `ProcessNodeInfo`, `TaskStatus`, `ProcessStatus`, `LogEntry`, `TaskId`, `KillSignal`, `SpawnOptions` — these were already designed as the immutable observation surface for the TUI and broadcast subscribers, separate from the `Mutex`/`Arc`/`JoinHandle`-laden live state. Crossing a process boundary doesn't change their role; we just add `Serialize`/`Deserialize` derives where missing (`LogEntry` already has `Serialize`).

**Convention:** when adding or refactoring a snapshot/value type in the engine, the change propagates to the wire automatically. That's intentional — same crate, same version, both ends recompile together. Do not introduce a parallel "wire-only" copy of an engine type. If you find yourself wanting to, ask whether the engine type itself should change instead.

The wire layer adds only the envelope and a few transport-only types:

```rust
// src/mcp/wire.rs

pub enum WireMessage {
    Request  { id: CorrelationId, body: Request },
    Response { id: CorrelationId, body: Result<Response, RpcError> },
    Event    (Event),
}

pub enum Request {
    ListTasks,
    SpawnTask        { name: String, args: Vec<String>, opts: SpawnOptions },
    KillTask         { task_id: TaskId, signal: KillSignal },
    KillProcess      { process_id: TaskId, signal: KillSignal },
    KillAll,
    GetLogs          { task_id: TaskId, since_seq: Option<u64>, until_seq: Option<u64>, limit: Option<u32>, filter: Option<String> },
    GrepLogs         { task_id: TaskId, pattern: String, limit: Option<u32>, scope: GrepScope },
    SubscribeLogs    { task_id: TaskId, filter: Option<String>, from_seq: Option<u64> },
    UnsubscribeLogs  { subscription_id: SubscriptionId },
}

pub enum Response {
    ListTasks       (Vec<TaskInfo>),
    SpawnTask       { task_id: TaskId, initial_seq: u64 },
    KillTask, KillProcess, KillAll,
    GetLogs         { entries: Vec<LogEntry>, next_seq: u64, has_more: bool },
    GrepLogs        { matches: Vec<LogEntry> },
    SubscribeLogs   { subscription_id: SubscriptionId },
    UnsubscribeLogs,
}

pub enum Event {
    Graph { snapshot: GraphSnapshot },
    Log   { subscription_id: SubscriptionId, entry: LogEntry },
}

pub enum RpcError {
    Engine(EngineError),     // engine-originated; nests today's existing error type
    BadRequest(String),      // wire-level: malformed params, args fail to parse, etc.
    FilterParse(String),     // log filter syntax error
    Internal(String),        // engine bug / panic recovered
}

pub enum GrepScope { Descendants, SelfOnly }

pub struct CorrelationId(u64);     // supervisor-allocated
pub struct SubscriptionId(u64);    // engine-allocated, monotonic per connection

// TaskInfo is small enough to live with the engine's Registry rather than the
// wire layer — added as `Registry::list_info() -> Vec<TaskInfo>`.
pub struct TaskInfo {
    pub name: String,
    pub group: String,
    pub description: Option<String>,
    pub args_help: Option<String>,
}
```

That's the complete list of types the wire layer adds. Everything else is reused from the engine.

### Method semantics

- **`SpawnTask`** — `initial_seq` is the global `LogStore` seq at the moment of spawn. The supervisor uses it as `from_seq` on a follow-up `SubscribeLogs` to close the spawn-then-subscribe race. `BadRequest` if args fail the task's clap parser; `NotFound` if name is unknown.
- **`KillTask` / `KillProcess`** — `KillSignal::Term` runs the existing cancel ladder; `KillSignal::Kill` runs it with `kill_timeout=0` (immediate SIGKILL on owned processes).
- **`KillAll`** — cancels every direct child of the engine's root. Root stays alive; the TCP session continues.
- **`GetLogs`** — entries from `task_id` and all descendants (tasks + processes), ascending by global `seq`. `since_seq` is exclusive, `until_seq` is inclusive. Default `limit` is 200; cap at 5000. Filter expressions are parsed by `src/log/filter.rs`; `FilterParse` on syntax error.
- **`GrepLogs`** — regex against `entry.message` if parsed, else `entry.raw`. Same scope semantics as `GetLogs` when `scope = Descendants`; `SelfOnly` restricts to entries whose `source == task_id`.
- **`SubscribeLogs`** — engine pushes `Event::Log` frames for every matching entry. With `from_seq`, engine replays `seq > from_seq` from the store first, then continues with live entries. Subscriptions die with the connection.

### Events

- **`Event::Graph`** — always-on, no subscribe lifecycle. Pushed on every engine lifecycle change (spawn, status, summary, process appearance/exit, readiness flip). The supervisor caches the latest snapshot per gen so it can answer `get_task` from cache plus a `GetLogs` call — no `GetTask` / `GetGraph` RPC needed.
- **`Event::Log`** — pushed only for active subscriptions. Engine-side filtering, so the wire only carries matches.

Loopback bandwidth isn't a constraint; sending full graph snapshots on every change is fine.

### LogStore changes

The protocol relies on a monotonic seq for cross-source cursoring (subscribe/paginate across descendants spans multiple sources). Today's `LogEntry.seq` is per-source. Move to engine-global; no separate per-source seq:

```rust
pub struct LogStore {
    // existing fields...
    next_seq: AtomicU64,   // 0 reserved as "before anything"
}

pub struct LogEntry {
    // unchanged shape; only meaning shifts:
    pub seq: u64,   // engine-global monotonic, assigned by LogStore::push
}
```

`LogStore::push` stamps `entry.seq` via `next_seq.fetch_add(1) + 1` before broadcast/storage. Pushers stop setting it. `OutputBuffer`'s upstream seq allocation goes away — entries flow through unstamped and leave stamped. Within a single source, global seq is also monotonically increasing (it's `fetch_add`), so the TUI's per-source rendering needs no migration; `entry.seq` is still the right cursor for in-source ordering.

The wire RPC handlers call into three new methods on `LogStore`:

```rust
impl LogStore {
    /// Range query by global seq, scoped to a source set + optional filter.
    /// Returns (entries, next_seq, has_more) per the wire contract.
    /// `since_seq` is exclusive, `until_seq` is inclusive.
    pub fn get_range(
        &self,
        sources: &HashSet<TaskId>,
        since_seq: Option<u64>,
        until_seq: Option<u64>,
        limit: usize,
        filter: Option<&FilterExpr>,
    ) -> (Vec<LogEntry>, u64, bool);

    /// Regex grep over message-or-raw, scoped to a source set.
    pub fn grep(
        &self,
        sources: &HashSet<TaskId>,
        pattern: &Regex,
        limit: usize,
    ) -> Vec<LogEntry>;

    /// Replay-then-live subscription, scoped to a source set with optional filter.
    /// Replay yields entries with `seq > from_seq`; live continues until drop.
    pub fn subscribe_with(
        &mut self,
        sources: HashSet<TaskId>,
        filter: Option<FilterExpr>,
        from_seq: Option<u64>,
    ) -> impl Stream<Item = LogEntry> + Send + 'static;
}
```

`subscribe_with` takes `&mut self`. Inside it: subscribe to the broadcast channel first (so the receiver only sees entries pushed _after_ this point), snapshot historical matching entries, yield historical-then-live. Because we hold `&mut self`, no `push()` runs during the call, so historical and live partition cleanly along the global seq with no dedup logic.

Source-set computation lives in the wire handler, not `LogStore`: the supervisor's cached `GraphSnapshot` knows the descendant set for any `task_id`, builds the `HashSet<TaskId>`, hands it to `LogStore`. Existing methods (`compose`, `compose_filtered`, `output_for_many`, `subscribe_filtered<F>`, `output`, `output_for`) keep their shapes for TUI/in-process callers; the wire methods are additive.

### Wire-up

Plain `tokio::net::TcpListener` / `TcpStream` on both ends. Framing via `tokio_util::codec::{Framed, LinesCodec}` (or a hand-rolled `BufReader::lines()` + `write_all` pair if we want one fewer dep). Each side wraps the codec in a tiny `send(&WireMessage)` / `recv() -> WireMessage` helper that goes through `serde_json` — the helper is the single discipline point that prevents anyone from accidentally sending pretty-printed JSON. The agent↔supervisor leg uses `rmcp` (official Rust MCP SDK) for stdio JSON-RPC; the supervisor↔engine leg is bespoke `serde_json`-over-TCP. The engine's `Event` stream is a direct forward of the existing `watch::Receiver<GraphSnapshot>` and `broadcast::Receiver<LogEntry>` — the transport adapter just serializes and writes. New engine state: a subscription registry (`HashMap<SubscriptionId, FilterExpr>`).

Supervisor-side per-gen state: in-flight correlation map (`HashMap<CorrelationId, oneshot::Sender<Result<Response, RpcError>>>`), latest cached `GraphSnapshot`, and the set of subscriptions it opened.

## Tool surface

Both compound and primitive tools — neither alone covers the agent use cases.

**Compound:**

- `run_task(name, args, timeout?, tail_n?)` — foreground; blocks until terminal status; returns the rendered task report (see "Task report"). The "build a thing and tell me how it went" call. Implemented in the MCP proxy: send `spawn_task` over RPC, get back `{task_id, initial_seq}`, open a `subscribe_logs` from that seq into a small tail buffer, await the graph event whose node hits a terminal state, render the report, return. No polling, no proxy-side state machine. Not exposed as an RPC primitive — the engine doesn't need to know `run_task` exists.

**Primitive:**

- `list_tasks` — reads `Registry::list()`.
- `spawn_task(name, args, timeout?)` → `{task_id}` — backgrounded, returns immediately.
- `kill_task(id, signal)` / `kill_process(id, signal)` — direct engine handle calls, routed by the address's `<top>`.
- `kill_all` — the agent's "stop everything I started." Supervisor fans out a kill to every live engine in parallel. After completion, all non-latest engines retire (their tasks are now terminal); the latest engine survives with no tasks. There is no MCP `quit` — the supervisor's lifetime is owned by the MCP session, not the agent.
- `get_graph` — current merged `GraphSnapshot`.
- `get_task(id, tail_n?)` — rendered task report (see "Task report"). Works on running or completed tasks.
- `get_logs(task_id, since_seq?, until_seq?, limit?, filter?)` → `{entries, next_seq, has_more}` — cursor-paged, bounded.
- `grep_logs(task_id, pattern, limit?, scope: descendants|self_only)` → matches.
- `get_build_status` → `{state, last_success_at?, last_failure_at?, last_failure_output?}` — current build state plus the cargo output of the most recent failure (full, not truncated). Lets the agent inspect build health without committing to a spawn; complements the build error returned by `spawn_task` / `run_task`.
- `install_skills(target_dir)` — agent-driven skills bootstrap. See Skills for the I/O contract, source layout, and shared implementation.

The `spawn_task` + `get_task` + `get_logs` + `kill_task` set is what makes the long-running interaction loop work: start a service, exercise it via other tools, query its logs, edit code, restart. That loop is the architectural justification for the whole frontend.

## Output access

Two layers, two shapes:

- **MCP surface (pull):** `get_logs` is cursor-paged. Stateless, bounded, fixed memory, agent decides when it has enough. The right shape for tools an LLM is composing — each call is independent.
- **RPC layer (push):** `subscribe_logs` is a streaming primitive on the supervisor↔engine TCP. Used internally by `run_task` to keep a tail buffer warm without polling, and available if a future MCP tool wants push semantics. Filtering happens engine-side in both cases; the same filter expression drives both (parsed by `src/log/filter.rs`).

The cursor primitive is the global `LogStore` seq introduced in the RPC protocol section — see "LogStore changes" there for the engine-side migration.

## Task report

Every task gets a structured report: a human-readable text block rendered by the MCP layer from engine-tracked fields. The same report is the body of `run_task` (returned when the task reaches a terminal state) and `get_task` (callable any time, on running or completed tasks). Tasks can optionally fill in a summary slot via `ctx.summary(s)`; the rest of the report is engine-derived.

This format exists for MCP consumers — LLMs that read the response as text and act on it. Raw graph consumers (`--engine`, `--cli`) work with the engine's structured types directly and don't need this rendering.

### Format

```
Task <id> <name> - <status>
Started: <local datetime>  Run time: <duration>
Stdout: <N> lines[, <format detection>]
Stderr: <N> lines[, <format detection>]
Events: <N> lines
Summary:
<task-authored summary, if set>
Last n lines:
<tail of the task's log, omitted if Summary is present>
```

- **`<id>`** — the task's supervisor-level identifier (the dotted `<top>.<task>` form from Identifiers and routing), rendered with a `t` prefix, e.g. `t42.7`. For a top-level task the form collapses to `t42`.
- **`<status>`** — terminal forms: `completed (exit 0)`, `failed: <reason>`, `cancelled`, `timed out`. Non-terminal forms: `running (setup)`, `running (ready)`. When non-terminal, the "Run time:" line gains a `(running)` suffix.
- **`Stdout` / `Stderr`** — line counts aggregated across all descendant processes' streams (the task itself has no OS-level output). Format suffix shows the dominant `ParsedContent` kind for that stream as `JSON 91%`, `CargoDiag 73%`, `Logfmt 65%`, etc. Omitted entirely if no non-`PlainText` kind clears 60%.
- **`Events`** — task-authored entries: tracing macros (`info!`/`error!`/etc.) and `ctx.println(...)`, aggregated across the task and all descendant tasks. No format detection — events are structured by definition. The bucket exists so task-authored output stays visible instead of getting lumped into stderr.
- **`Summary:`** — task-authored. Present if `ctx.summary(s)` was called at any point during the task's lifetime. Last-write-wins.
- **`Last n lines:`** — fallback when `Summary` is absent. The same human-readable rendering produced by `--cli` mode for this task's subtree (entries from all descendant sources interleaved by `received_at`), tailed to `N`. `N` defaults to 50; configurable per `run_task` / `get_task` call. Omitted when `Summary` is set — the agent asked for the report, not both.

### Engine state needed

- `TaskExecution::summary: Mutex<Option<String>>` — written by `ctx.summary(s)`, read by the report renderer. Last-write-wins, no size cap (trust the task author for v1).
- `TaskExecution::started_at: chrono::DateTime<Local>` — set when the body's tokio task is spawned.
- `TaskExecution::ended_at: Mutex<Option<chrono::DateTime<Local>>>` — set when status moves to a terminal variant (`Done` / `Failed` / `Cancelled` / `Timeout`). Status mutator writes both fields together.
- Stream/format aggregation comes from walking `LogStore` entries for the id set returned by `source_ids_for(task_id)`. No new state; the renderer counts on demand. Cheap at human-scale log volumes; revisit if it bites.

### `ctx.summary` API

```rust
impl TaskContext {
    /// Set the task's summary. Last write wins.
    pub fn summary(&self, s: impl Into<String>);
}
```

Sync — it's a memory write, no I/O. Calls after the task reaches a terminal status are accepted (the field on `TaskExecution` outlives the body's tokio task) but unusual; no special-case handling. Setting summary publishes a fresh graph snapshot so subscribers see the change without waiting for the next lifecycle event.

### Surface points

- **`run_task` MCP response** — rendered report as the response text. Returned when the task reaches a terminal state.
- **`get_task` MCP response** — same rendered report. Works at any state; running-vs-terminal is encoded in the report itself.
- **`TaskNode.summary: Option<String>`** in `GraphSnapshot` — the task-authored summary string, surfaced on the node so the TUI and any other graph observer can read it without going through MCP. The full report envelope is MCP-only.

The renderer lives in `src/mcp/report.rs`. It takes an `EngineHandle` + `TaskId` + tail `n`, walks the snapshot and `LogStore`, and produces a `String`. The motivating example — a `cargo build` task that post-processes errors/warnings/failing crate into a few lines an agent can act on — drives the shape: the agent gets one tight read with everything it needs, no second tool call to figure out what happened.

## Change detection

The current "shell out to `cargo build` on every invocation" model is acceptable for one-shot CLI use because the human is paying the latency tax of typing a command. MCP changes the contract: a tool call would indirectly pay it, and an agent will hammer this. Cargo's incremental check stat()s the dep graph and parses Cargo.lock — on a clean workspace this is ~150-400ms, dead time per call.

**Decision:** the supervisor owns change detection. It runs a file watcher and only invokes cargo when something semantically changed. Tool calls hit a warm, up-to-date child instantly.

Flow:

```
supervisor startup:
  spawn gen 1 with --engine --start-task-id 1; state = Rebuilding
  watch for {"port": N} on stdout OR child exit
    on port line: open TCP; promote to "latest"; state = Idle
    on early exit: capture stderr; state = LastBuildFailed
  start notify watcher on RUNME.rs files (and sibling .rs files)

on file event:
  debounce 200ms
  spawn next gen with --engine --start-task-id <past last used>; state = Rebuilding
  watch for port line OR child exit, same as startup
    on success: promote to "latest"; older gens stay alive (with their LogStores)
    on failure: state = LastBuildFailed; existing live gens are unaffected
  re-discover RUNME.rs files for the watcher; rebuild watch set if any appeared/disappeared
  (the child engine does its own discovery for compilation; supervisor discovery
  is purely for the file watcher's input)

on gen event (from any live gen's graph stream):
  if the gen never had tasks AND a newer gen has been promoted to latest:
    retire it immediately (it's holding nothing of value).
  otherwise: leave it alive — its tasks are the only retention we have.

on tool call (request from agent):
  if request carries an id whose <top> is in a (rare) retired never-had-tasks
      engine, or is unknown to the supervisor: return not_found
  if request carries an id whose <top> is in a live engine: route to that engine
  if request is a new spawn (spawn_task / run_task):
    if rebuilding: block until done
    if last-rebuild-failed: return error with cargo output (no fallback to old gen)
    else: forward to latest engine
```

**What's watched:**

- Every discovered RUNME.rs.
- Every `.rs` file in each RUNME.rs's directory (a RUNME.rs can `mod foo;` a sibling). Same .gitignore rules as discover.

**What's not watched:**

- Workspace `Cargo.toml` files. Frontmatter `[dependencies]` is the primary dep mechanism; the workspace `Cargo.toml` edge case can be revisited if it bites.
- The generated workspace in the cache dir (we own it).

**The hash-and-skip-cargo optimization** — computing a hash of all RUNME.rs files plus frontmatter and skipping cargo entirely when unchanged — is _not_ in v1. The file watcher already eliminates the per-call cost; the additional optimization is a fixed cost only paid on watcher-triggered rebuilds.

`notify` is the watcher crate. macOS FSEvents coalesces aggressively — 200ms debounce is the right window; shorter sees too many spurious events.

## Build state machine

The supervisor has a single build state, separate from per-engine lifecycle. Only **spawn-shaped calls** (`spawn_task`, `run_task`) care about it; calls that target an existing top-task (`kill_task`, `get_task`, `get_logs`, etc.) route directly to the owning engine and don't wait for a build.

Tools split into two groups based on whether they need the current build:

- **Need-current-build:** `spawn_task`, `run_task`, `list_tasks`. These reflect what the user's current RUNME.rs files define — the agent has to see live code, not stale code.
- **Existing-state:** `kill_task`, `kill_process`, `kill_all`, `get_task`, `get_logs`, `grep_logs`, `get_graph`. These reference already-running tasks in still-live engines; they're indifferent to whether the latest build compiles.

`get_build_status` is always available regardless of state. `install_skills` doesn't touch engines at all.

| State           | Need-current-build (spawn, run_task, list_tasks) | Existing-state tools                  |
| --------------- | ------------------------------------------------ | ------------------------------------- |
| Idle            | Forward to latest engine.                        | Route normally by `<top>`.            |
| Rebuilding      | Block until transition.                          | Route normally.                       |
| LastBuildFailed | **Return error with cargo output head** (~12 lines, with hint to call `get_build_status` for full output). No fallback to a previous build. | Route normally. |

Transitions:

- `Idle` → `Rebuilding` on debounced file event (or on initial startup, before any engine has connected).
- `Rebuilding` → `Idle` when the spawned `rnme --engine` child prints `{"port": <u16>}` on stdout. Supervisor opens TCP, marks new engine as latest. The engine handles its own discover/compile/exec internally; from the supervisor's view this is just "spawn child, watch stdout for port line or child exit."
- `Rebuilding` → `LastBuildFailed` when the child exits before printing the port line. Supervisor captures the child's stderr (cargo errors, panics, whatever it produced) into `last_failure_output`. Existing live engines are unaffected.
- `LastBuildFailed` → `Rebuilding` on next file event.
- `LastBuildFailed` → `Idle` on subsequent rebuild success.

The agent is never disconnected. Top-tasks remain reachable for as long as their owning engine is live; once an engine retires, ids whose `<top>` lived in it return `not_found` (see Identifiers and routing).

The "fail spawns when builds are broken, never silently use stale code" stance is what makes the edit-test loop honest: an agent editing RUNME.rs and calling tasks gets the cargo error directly on the failing call, never a successful spawn that ran last-week's bytes. `get_build_status` is the read-only check for agents that want to inspect build health without committing to a spawn.

## Skills

The MCP surface is the runtime; skills are how an agent learns to use it. A library of skills, versioned with the binary, teaches connecting agents both how to operate tasks (the MCP primitives) and how to author them (RUNME.rs idioms — `cmd!`, args/clap, frontmatter deps, readiness, group structure, file placement). Without the skills, an agent connecting to `rnme --mcp` has the tools but not the conventions; the skills close that gap and are the discovery vector for the authoring use case described in Goal.

### Source layout

Skills live in `docs/manual/` in the repo, one directory per skill, in Claude Code's expected layout:

```
docs/manual/
  rnme-operate/
    SKILL.md
    <progressive-disclosure>.md
  rnme-author/
    SKILL.md
    <progressive-disclosure>.md
```

`SKILL.md` is required: YAML frontmatter with `name` + `description` (the description is the trigger string the harness uses to decide when to load) plus a markdown body. Sibling files are deeper content the skill body may reference. The manual files _are_ the skills — humans browsing the repo see exactly what gets installed, and the doc folder doubles as the agent-facing reference. No transform layer between source and install in v1.

The binary embeds the directory tree via `include_dir!` (or equivalent) so installation has no runtime dependency on the repo layout — `cargo install rnme` ships with the skills bundled.

### Installation

Two entrypoints share one implementation in `src/mcp/skills.rs`:

- **`rnme :install_skills <target>`** — builtin task. Human-driven; runs from a terminal during project setup. Routes to the same `install_to(path)` library function as the MCP tool. (Underscore, not hyphen — the `#[rnme::task]` macro derives the registered task name from the function ident, and Rust function names cannot contain hyphens.)
- **MCP tool `install_skills(target_dir)`** — agent-driven; called after connecting to `rnme --mcp`.

Both copy the embedded tree to `<target>/rnme/<skill>/`. The `rnme/` namespace dir scopes the install — uninstall is `rm -rf <target>/rnme/`, and skill names can't collide with skills the user wrote themselves. Re-running overwrites, so re-installing on each rnme upgrade keeps the installed skills in sync with the binary version.

Contract:

```
install_skills(target_dir: string) -> {
  target:    string,            // canonical absolute path of <target_dir>/rnme/
  installed: [string, ...],     // skill names that landed
}
```

- `target_dir` accepts both relative (resolved against supervisor cwd) and absolute paths. The response always reports the canonicalized absolute path so the agent never has to re-resolve.
- Missing `target_dir` is auto-created (recursive `mkdir`). Errors only when the path exists as a non-directory or creation fails (permissions, disk full, etc.).
- The install is an **atomic replace** of `<target>/rnme/`: write to a sibling temp dir, then rename over any existing `rnme/`. Avoids half-written states; re-runs always produce a clean tree. Hand-edits inside `<target>/rnme/` are overwritten — the install dir is owned by rnme, not by the user.
- Concurrent calls serialize through a supervisor-side mutex. The second call observes whatever the first wrote; usually a no-op since contents match.
- Errors surface as MCP tool failures with a single string explaining which step broke (`target path is a file`, `permission denied creating <path>`, etc.).

### Discovery from MCP

The supervisor populates the standard `instructions` slot in its `initialize` response with a short blurb describing the available skills and prompting the agent to install them via `install_skills(target_dir)`. The agent passes the path appropriate to its framework — for Claude Code that's `<project>/.claude/skills/`. No auto-detect or path guessing on the supervisor side: the agent already knows its own conventions, and the MCP channel is the right place to ask rather than infer.

### Out of scope for v1

- Transforms for non-Claude frameworks (Cursor `.mdc`, `AGENTS.md`, etc.). Possible later as a render layer over the same source tree; v1 ships the SKILL.md format only.
- Stale-version detection (binary checks installed file versions and prompts re-install). Manual re-run after upgrade is fine.
- Per-project skill customization or templating. Skills ship as-is.

## What we are explicitly not doing

- No web UI. No second public API. No auth. No semver discipline on the RPC. Add when needed.
- No SSE / HTTP long-poll / WebSocket. A single TCP connection per gen carrying JSONL is enough.
- No Windows. No Unix-socket fallback.
- No supervisor-side mirroring of retired-gen state. Once a gen retires, its tasks/logs are gone. Future supervisor feature if it bites.
- No per-task MCP tool generation with synthesized JSON schemas. Generic `spawn_task(name, args: string[])` — agents read `--help` if they need argument detail. JSON-payload args via `serde::Deserialize` is plausible for tasks that want it, but not v1.
- No reconnection logic on the agent side. The supervisor↔child TCP connection is loopback; if it drops, something is very wrong.
- No streaming MCP responses. The MCP tool surface is request/response — long-running interaction is `spawn_task` + `get_logs` cursor pulls. (Push exists on the RPC, just not surfaced through MCP.)

## Open / next

- **Reference the manual from `CLAUDE.md`.** Once `docs/manual/` has at least skeleton content, update the project `CLAUDE.md` to point at it as the source-of-truth reference for RUNME.rs authoring and MCP usage, and to mention `rnme :install_skills` as the way to materialize the manual as agent skills.
