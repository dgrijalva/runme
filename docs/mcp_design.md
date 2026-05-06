# MCP Frontend Design

The MCP (Model Context Protocol) frontend exposes rnme's task runtime to coding agents. This document captures the architecture decisions made before implementation; for the underlying engine, see `runtime_engine_design.md`.

## Goal

Give an agent a stable interface for driving rnme over the lifetime of a session: list tasks, run them (foreground or backgrounded), monitor their state, query their output. The interface needs to survive RUNME.rs file edits during the session — agents can't be told "restart your MCP server" mid-conversation, and many MCP clients disable servers that crash or exit unexpectedly.

## Topology

```
agent ─stdio─▶ rnme --mcp ─┬─WebSocket─▶ rnme --ws  (gen 1, retiring as tasks drain)
                  │         ├─WebSocket─▶ rnme --ws  (gen 2, latest — receives new spawns)
                  │         └─WebSocket─▶ rnme --ws  (gen N, spawned on next rebuild)
                  │
                  │  on RUNME.rs change: rebuild cargo, spawn next gen,
                  │                      route new spawns there;
                  │                      old gens retire when their tasks complete
```

Two new dispatch arms in `cli.rs`:

- **`--ws`** — engine + WebSocket server bound to `127.0.0.1:0` (OS-assigned port). Headless. No TUI, no stdio forwarding. The "engine daemon" mode. On startup, prints connection info as a single JSON object on stdout (the _only_ thing it ever writes to stdout); errors go to stderr. Gen-unaware — it allocates IDs from its own atomic, starting at 1, the same as today.
- **`--mcp`** — MCP server on stdio (parent agent). Maintains a list of live generations, each backed by an `rnme --ws` child. Translates each MCP tool call into a WebSocket message routed to the correct gen, wrapping/unwrapping IDs at the boundary (see Generations). Watches RUNME.rs files; on change, spawns a new gen as latest. Child stderr is dropped (or forwarded later if useful).

Like `--tui` and `--cli`, these are bare flags; passing more than one mode flag is rejected by the arg parser.

## Why this shape

The shape was reached by removing complexity from a more ambitious starting point:

1. **One RPC layer, not two.** An earlier draft separated a "supervisor↔engine internal protocol" from a "public RPC for external consumers." There are no external consumers today — only MCP. A single internal-but-honest RPC, versioned with the rest of the crate, is enough. If a web UI or remote agent shows up later, that's the right time to think about stability and auth — not now.
2. **No loopback abstraction.** macOS/Linux only. TCP `127.0.0.1` is fine; no Unix sockets, no Windows.
3. **No streaming subscriptions over HTTP/SSE.** WebSocket is one persistent connection that handles request/response _and_ server-pushed events with the same message machinery. Avoids long-polling, avoids a separate streaming endpoint shape, avoids gRPC weight.
4. **TUI stays in-process.** Tempting to make the TUI a client of the RPC for symmetry. Don't. The TUI's tight coupling to `watch::Receiver<GraphSnapshot>` and `broadcast::Receiver<LogEntry>` is a feature, not an accident. `rnme` (interactive) and `rnme --mcp` (headless+agent) are sibling entry points, not stacked.
5. **Engine-as-child for MCP, not in-process.** Restarting an MCP server without restarting the agent is unreliable in practice — many clients disable MCPs that exit. Keeping MCP alive across rebuilds means the engine has to be a separate process the supervisor can manage. With generational supervision (see Generations), engine-as-child also lets long-running tasks survive unrelated edits — old generations stay alive until their tasks drain naturally, while new spawns route to the rebuilt latest gen. The cost is one extra process per live gen + the WebSocket; the benefit is the agent never sees a disconnect, and editing one task doesn't kill an unrelated running service.

## Generations

The supervisor maintains a list of engine children — generations — rather than kicking the single child between rebuild cycles. Each generation runs on a specific build of the user's RUNME.rs files; tasks spawned in gen N execute the code that was current when gen N was built.

### Why

A running task uses the code it was built against. If `npm run dev` is running on gen 1's build, an unrelated edit shouldn't kill it. New spawns after the edit get gen 2's build with the fresh code; gen 1's `npm run dev` keeps going on its old code until something stops it. The agent's existing task IDs stay live, and long-running services survive editing — which is the actual user pain point.

The cost is contained to the supervisor: engine code is unchanged, and the agent doesn't see generations as a concept.

### Lifecycle

```
file event → debounce 200ms → cargo rebuild
  on success: spawn gen N+1; it becomes "latest"; new spawns route there
  on failure: keep current latest gen; surface error on next tool response

per-gen retirement:
  - latest gen is never retired by task completion
  - a gen that had tasks: when ALL its tasks are terminal AND it is not latest,
    start a cooldown timer (default 15 min, --gen-cooldown on --mcp).
    any access (get_task, get_logs, kill_task, etc.) resets the timer.
    on expiry, kill the child and drop supervisor-side state.
  - a gen that never had tasks (spawned, then immediately eclipsed by another
    build): retire immediately — no logs worth keeping.
```

The latest gen is never retired by tasks completing — there's always a current gen ready for new spawns. Older gens drain into a cooldown window so the natural "run a task, immediately ask about it" agent flow doesn't lose data. The sliding TTL means an agent actively reading a gen's logs keeps it alive; one that walked away lets it retire.

### Identifiers and routing

Engine code keeps allocating `TaskId`s and log seqs from 1, in its own address space, exactly as today. The supervisor wraps every ID with the **generation number** before exposing it through MCP and unwraps on the way back. External wire format is a dotted string `<gen>.<engine-internal-id>`:

```
TaskId   external: "1.7"        → gen 1, engine-internal task id 7
LogSeq   external: "2.42351"    → gen 2, engine-internal seq 42351
```

Strings instead of packed integers because they read cleanly in agent-facing tools — `t1.7` is meaningfully better than `t100000000000007` when an agent is composing follow-up calls. The supervisor parses on inbound requests and formats on outbound snapshots/entries; engine-internal types are unchanged. Generations are `u16` (65 535 per supervisor session — plenty); engine-internal ids stay `u64`.

- **Tool calls that take an ID** (`kill_task`, `get_task`, `get_logs`, `kill_process`, etc.) — supervisor splits on `.`, parses the gen, forwards the engine-internal id to that gen's child.
- **Snapshots and log entries flowing from a gen back to the supervisor** — supervisor formats every embedded id as `"<gen>.<engine-id>"` before forwarding to the agent.
- **Reserved id `"0.0"`** — the supervisor's meta-root. The supervisor presents a unified `GraphSnapshot` to the agent: a synthetic root (`"0.0"`) whose children are the union of every live gen's top-level running tasks (each gen's engine-internal synthetic root is hidden in the merge). The agent sees one task graph, not N.
- **Stale IDs** — an id with a retired-gen prefix gets a clean `not_found` error from the supervisor, without crossing the WebSocket. Same goes for log cursors with retired-gen prefixes.
- **Malformed IDs** — anything that doesn't parse as `<u16>.<u64>` (or the special `"0.0"`) gets `bad_request`.

This means **no engine code change** beyond what's needed for the WebSocket itself. Generational logic lives entirely in the supervisor: a small wrap/unwrap layer at the WebSocket boundary plus the routing table.

### What survives a generation's retirement: nothing

When a gen finally retires (cooldown expired, no recent activity), its `LogStore` and graph state die with the child process. Tasks that were running in it are already terminal; their final reports and logs vanish. Agent queries against retired-gen IDs get `not_found`.

This is a real semantic change from today's "completed tasks stay forever" property: completed tasks stay only until their gen's cooldown expires. The cooldown is the safety net for the common "run a task, ask about it shortly after" pattern; an agent that needs longer-term retention should call `get_task` and capture the rendered report while the gen is still live (or keep accessing the gen, which extends the TTL).

A future supervisor feature could mirror retired-gen state (final reports + log tails) into supervisor memory for unbounded retention. Not v1.

## RPC protocol

Loopback WebSocket between the supervisor and one engine child per gen. Both ends are the same crate at the same version, so the protocol is just a shared Rust enum serialized with `serde_json`. No JSON shape spec, no semver, no auth — change the enum, both sides recompile together.

**IDs on the WebSocket are raw engine types** (`TaskId`, the global `LogStore` seq). The gen-prefix wrapping (see Generations) is purely an MCP-boundary concern; the engine has no idea which generation it is.

### Single source of truth: engine types ARE wire types

The wire protocol uses the engine's existing snapshot/value types directly. `GraphSnapshot`, `TaskNode`, `ProcessNodeInfo`, `TaskStatus`, `ProcessStatus`, `LogEntry`, `TaskId`, `KillSignal`, `SpawnOptions` — these were already designed as the immutable observation surface for the TUI and broadcast subscribers, separate from the `Mutex`/`Arc`/`JoinHandle`-laden live state. Crossing a process boundary doesn't change their role; we just add `Serialize`/`Deserialize` derives where missing (`LogEntry` already has `Serialize`).

**Convention:** when adding or refactoring a snapshot/value type in the engine, the change propagates to the wire automatically. That's intentional — same crate, same version, both ends recompile together. Do not introduce a parallel "wire-only" copy of an engine type. If you find yourself wanting to, ask whether the engine type itself should change instead.

The wire layer adds only the envelope and a few transport-only types:

```rust
// src/mcp/wire.rs

pub enum WsMessage {
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

That's the complete list of types the WebSocket layer adds. Everything else is reused from the engine.

### Method semantics

- **`SpawnTask`** — `initial_seq` is the global `LogStore` seq at the moment of spawn. The supervisor uses it as `from_seq` on a follow-up `SubscribeLogs` to close the spawn-then-subscribe race. `BadRequest` if args fail the task's clap parser; `NotFound` if name is unknown.
- **`KillTask` / `KillProcess`** — `KillSignal::Term` runs the existing cancel ladder; `KillSignal::Kill` runs it with `kill_timeout=0` (immediate SIGKILL on owned processes).
- **`KillAll`** — cancels every direct child of the engine's root. Root stays alive; the WebSocket session continues.
- **`GetLogs`** — entries from `task_id` and all descendants (tasks + processes), ascending by global `seq`. `since_seq` is exclusive, `until_seq` is inclusive. Default `limit` is 200; cap at 5000. Filter expressions are parsed by `src/log/filter.rs`; `FilterParse` on syntax error.
- **`GrepLogs`** — regex against `entry.message` if parsed, else `entry.raw`. Same scope semantics as `GetLogs` when `scope = Descendants`; `SelfOnly` restricts to entries whose `source == task_id`.
- **`SubscribeLogs`** — engine pushes `Event::Log` frames for every matching entry. With `from_seq`, engine replays `seq > from_seq` from the store first, then continues with live entries. Subscriptions die with the connection.

### Events

- **`Event::Graph`** — always-on, no subscribe lifecycle. Pushed on every engine lifecycle change (spawn, status, summary, process appearance/exit, readiness flip). The supervisor caches the latest snapshot per gen so it can answer `get_task` from cache plus a `GetLogs` call — no `GetTask` / `GetGraph` RPC needed.
- **`Event::Log`** — pushed only for active subscriptions. Engine-side filtering, so the wire only carries matches.

Loopback bandwidth isn't a constraint; sending full graph snapshots on every change is fine.

### LogStore changes

The protocol relies on a **global** monotonic seq for cross-source cursoring (subscribe/paginate across descendants spans multiple sources). The current `LogEntry.seq` is per-source and used by the TUI for in-source ordering. Keep that, add a global one alongside:

```rust
pub struct LogStore {
    // existing fields...
    next_global_seq: AtomicU64,
}

pub struct LogEntry {
    // existing fields, with semantic shift:
    pub seq: u64,           // now: global monotonic — used by RPC cursors
    pub source_seq: u64,    // NEW: per-source ordering, replaces today's `seq`
}
```

In-tree consumers of `LogEntry.seq` that wanted per-source ordering migrate to `source_seq`. `seq` becomes the cross-source cursor.

### Wire-up

`axum` for the engine side (`WebSocketUpgrade`), `tokio-tungstenite` for the supervisor side. The engine's `Event` stream is a direct forward of the existing `watch::Receiver<GraphSnapshot>` and `broadcast::Receiver<LogEntry>` — the WebSocket adapter just serializes and writes. New engine state: a subscription registry (`HashMap<SubscriptionId, FilterExpr>`).

Supervisor-side per-gen state: in-flight correlation map (`HashMap<CorrelationId, oneshot::Sender<Result<Response, RpcError>>>`), latest cached `GraphSnapshot`, and the set of subscriptions it opened.

## Tool surface

Both compound and primitive tools — neither alone covers the agent use cases.

**Compound:**

- `run_task(name, args, timeout?, tail_n?)` — foreground; blocks until terminal status; returns the rendered task report (see "Task report"). The "build a thing and tell me how it went" call. Implemented in the MCP proxy: send `spawn_task` over RPC, get back `{task_id, initial_seq}`, open a `subscribe_logs` from that seq into a small tail buffer, await the graph event whose node hits a terminal state, render the report, return. No polling, no proxy-side state machine. Not exposed as an RPC primitive — the engine doesn't need to know `run_task` exists.

**Primitive:**

- `list_tasks` — reads `Registry::list()`.
- `spawn_task(name, args, timeout?)` → `{task_id}` — backgrounded, returns immediately.
- `kill_task(id, signal)` / `kill_process(id, signal)` — direct engine handle calls, routed by gen prefix.
- `kill_all` — the agent's "stop everything I started." Supervisor fans out `kill_all` to every live gen in parallel. After completion, all non-latest gens retire (their tasks are now terminal); the latest gen survives with no tasks. There is no MCP `quit` — the supervisor's lifetime is owned by the MCP session, not the agent.
- `get_graph` — current `GraphSnapshot`.
- `get_task(id, tail_n?)` — rendered task report (see "Task report"). Works on running or completed tasks.
- `get_logs(task_id, since_seq?, until_seq?, limit?, filter?)` → `{entries, next_seq, has_more}` — cursor-paged, bounded.
- `grep_logs(task_id, pattern, limit?, scope: descendants|self_only)` → matches.

The `spawn_task` + `get_task` + `get_logs` + `kill_task` set is what makes the long-running interaction loop work: start a service, exercise it via other tools, query its logs, edit code, restart. That loop is the architectural justification for the whole frontend.

## Output access

Two layers, two shapes:

- **MCP surface (pull):** `get_logs` is cursor-paged. Stateless, bounded, fixed memory, agent decides when it has enough. The right shape for tools an LLM is composing — each call is independent.
- **RPC layer (push):** `subscribe_logs` is a streaming primitive on the WebSocket. Used internally by `run_task` to keep a tail buffer warm without polling, and available if a future MCP tool wants push semantics. Filtering happens engine-side in both cases; the same filter expression drives both (parsed by `src/log/filter.rs`).

The cursor primitive is the global `LogStore` seq introduced in the RPC protocol section — see "LogStore changes" there for the engine-side migration.

## Task report

Every task gets a structured report: a human-readable text block rendered by the MCP layer from engine-tracked fields. The same report is the body of `run_task` (returned when the task reaches a terminal state) and `get_task` (callable any time, on running or completed tasks). Tasks can optionally fill in a summary slot via `ctx.summary(s)`; the rest of the report is engine-derived.

This format exists for MCP consumers — LLMs that read the response as text and act on it. Raw graph consumers (`--ws`, `--cli`) work with the engine's structured types directly and don't need this rendering.

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

- **`<id>`** — the task's supervisor-level identifier (the dotted `<gen>.<engine-id>` form from the Generations section), rendered as `t<gen>.<engine-id>`, e.g. `t1.7`.
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
  discover RUNME.rs files
  cargo build (cold)
  spawn gen 1 with --ws; mark it "latest"
  start notify watcher on RUNME.rs files (and sibling .rs files)

on file event:
  debounce 200ms
  rebuild via existing compile.rs path
  if success: spawn next gen with --ws; promote it to "latest"
              older gens stay alive until their tasks drain
  if failure: keep current latest gen as-is; surface error on next tool response
  re-discover and rebuild watch set if RUNME.rs files appeared/disappeared

on gen event (from any live gen's WebSocket graph stream):
  if all of that gen's tasks are terminal AND it is not the latest gen:
    if the gen had tasks: start/refresh its cooldown timer
    if the gen never had tasks: retire immediately

on cooldown expiry:
  kill child, drop supervisor-side state for the gen

on tool call that targets a cooldown-pending gen:
  reset its cooldown timer (sliding TTL)

on tool call (request from agent):
  if request carries an ID with a retired-gen prefix: return not_found
  if request carries an ID with a live-gen prefix: route to that gen
  if request is a new spawn:
    if rebuilding: block until done
    if last-rebuild-failed: error with build output
    else: forward to latest gen
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

The supervisor has a single build state, separate from per-gen lifecycle. Only **new spawns** care about it; calls that target a specific live gen (`kill_task`, `get_task`, `get_logs`, etc.) route directly and don't wait for a build.

| State             | New-spawn behavior                                                  |
| ----------------- | ------------------------------------------------------------------- |
| Idle              | Forward to latest gen.                                              |
| Rebuilding        | Block until transition.                                             |
| LastBuildFailed   | Forward to latest gen (the previous successful one), tag the response with the build error. |

Transitions:

- `Idle` → `Rebuilding` on debounced file event.
- `Rebuilding` → `Idle` on successful build + new gen spawned + connected (wait for connection-info JSON on the child's stdout, open WebSocket, mark new gen as latest).
- `Rebuilding` → `LastBuildFailed` on cargo failure. Latest gen is unchanged; existing live gens are unaffected.
- `LastBuildFailed` → `Rebuilding` on next file event.
- `LastBuildFailed` → `Idle` on subsequent rebuild success.

The agent is never disconnected. Per-gen tasks remain reachable for as long as their gen is live; once a gen retires, IDs prefixed with that gen return `not_found` (see Generations).

## What we are explicitly not doing

- No web UI. No second public API. No auth. No semver discipline on the RPC. Add when needed.
- No SSE / HTTP long-poll. WebSocket carries everything.
- No Windows. No Unix-socket fallback.
- No supervisor-side mirroring of retired-gen state. Once a gen retires, its tasks/logs are gone. Future supervisor feature if it bites.
- No per-task MCP tool generation with synthesized JSON schemas. Generic `spawn_task(name, args: string[])` — agents read `--help` if they need argument detail. JSON-payload args via `serde::Deserialize` is plausible for tasks that want it, but not v1.
- No reconnection logic on the agent side. The MCP proxy↔child WebSocket is loopback; if it drops, something is very wrong.
- No streaming MCP responses. The MCP tool surface is request/response — long-running interaction is `spawn_task` + `get_logs` cursor pulls. (Push exists on the RPC, just not surfaced through MCP.)

## Open / next

- **`LogStore` Rust API.** The wire shapes for `get_logs` / `subscribe_logs` / `grep_logs` are fixed (see RPC protocol); what remains is the Rust API on `LogStore`: signatures for range-by-global-seq, filtered live subscription that emits matched entries, and the global-vs-per-source seq migration of existing call sites.
- **Supervisor merge of `GraphSnapshot` across live gens.** Concrete shape of the unified snapshot the agent sees: how `"0.0"` (supervisor meta-root) lists gen-prefixed children, whether per-gen synthetic roots are hidden or surfaced, how concurrent snapshot updates from multiple gens coalesce into one `watch::changed()` tick on the supervisor side.
