# MCP Frontend Design

The MCP (Model Context Protocol) frontend exposes rnme's task runtime to coding agents. This document captures the architecture decisions made before implementation; for the underlying engine, see `runtime_engine_design.md`.

## Goal

Give an agent a stable interface for driving rnme over the lifetime of a session: list tasks, run them (foreground or backgrounded), monitor their state, query their output. The interface needs to survive RUNME.rs file edits during the session — agents can't be told "restart your MCP server" mid-conversation, and many MCP clients disable servers that crash or exit unexpectedly.

## Topology

```
agent ─stdio─▶ rnme --mcp ─WebSocket on 127.0.0.1:N─▶ rnme --rpc (engine child)
                  │                                          │
                  │  on RUNME.rs change:                     │
                  │  kill + respawn child  ───────────────────┘
```

Two new dispatch arms in `cli.rs`:

- **`--ui rpc`** — engine + WebSocket server bound to `127.0.0.1:0` (OS-assigned port). Headless. No TUI, no stdio forwarding. The "engine daemon" mode. On startup, prints connection info as a single JSON object on stdout (the _only_ thing it ever writes to stdout); errors go to stderr.
- **`--ui mcp`** — MCP server on stdio (parent agent). Spawns a child `rnme --ui rpc`, reads its connection info, opens a WebSocket. Translates each MCP tool call into a WebSocket message. Watches RUNME.rs files; on change, kills the child and respawns. Child stderr is dropped (or forwarded later if useful).

## Why this shape

The shape was reached by removing complexity from a more ambitious starting point:

1. **One RPC layer, not two.** An earlier draft separated a "supervisor↔engine internal protocol" from a "public RPC for external consumers." There are no external consumers today — only MCP. A single internal-but-honest RPC, versioned with the rest of the crate, is enough. If a web UI or remote agent shows up later, that's the right time to think about stability and auth — not now.
2. **No loopback abstraction.** macOS/Linux only. TCP `127.0.0.1` is fine; no Unix sockets, no Windows.
3. **No streaming subscriptions over HTTP/SSE.** WebSocket is one persistent connection that handles request/response _and_ server-pushed events with the same message machinery. Avoids long-polling, avoids a separate streaming endpoint shape, avoids gRPC weight.
4. **TUI stays in-process.** Tempting to make the TUI a client of the RPC for symmetry. Don't. The TUI's tight coupling to `watch::Receiver<GraphSnapshot>` and `broadcast::Receiver<LogEntry>` is a feature, not an accident. `rnme` (interactive) and `rnme --ui mcp` (headless+agent) are sibling entry points, not stacked.
5. **Engine-as-child for MCP, not in-process.** Restarting an MCP server without restarting the agent is unreliable in practice — many clients disable MCPs that exit. Keeping MCP alive across rebuilds means the engine has to be a separate process the supervisor can kill and respawn. The cost is one extra process + the RPC; the benefit is the agent never sees a disconnect when the user edits RUNME.rs.

## RPC protocol

WebSocket. JSON messages. Three message families, discriminated by a `type` field:

1. **Request/response** — proxy → engine call, engine → proxy reply. Each carries a `correlation_id`; replies echo it back.
2. **Graph events** — engine → proxy, unsolicited. Pushed for every status transition, summary update, and node add/remove. Always on; the proxy receives them as soon as it connects. There's no `subscribe_graph` — there's exactly one consumer and no reason to invent a lifecycle.
3. **Log events** — engine → proxy, in response to a `subscribe_logs(task_id, filter, from_seq?)` request. Each subscription is identified by a `subscription_id` returned in the response; subsequent log events carry that id. Engine-side filtering, so the wire only carries matches. The `from_seq` parameter closes the spawn-then-subscribe race: `spawn_task` returns `{task_id, initial_seq}`, the proxy opens a subscription `from_seq=initial_seq`, no entries are missed.

The protocol is internal — it's a contract between two binaries built from the same crate. No semver, no OpenAPI, no auth. Add a new method by adding a new variant to the message enum on both ends. If a non-MCP consumer ever shows up, that's when discipline becomes interesting.

`axum`'s `WebSocketUpgrade` is the natural server side; `tokio-tungstenite` for the client. The graph and log event streams are direct forwards of the engine's existing `watch::Receiver<GraphSnapshot>` and `broadcast::Receiver<LogEntry>` — no new state to maintain.

## Tool surface

Both compound and primitive tools — neither alone covers the agent use cases.

**Compound:**

- `run_task(name, args, timeout?)` — foreground; blocks until terminal status; returns `{status, exit_code, summary?, last_n_lines}`. The "build a thing and tell me how it went" call. Implemented in the MCP proxy: send `spawn_task` over RPC, get back `{task_id, initial_seq}`, open a `subscribe_logs` from that seq into a small tail buffer, await the graph event whose node hits a terminal state, return. No polling, no proxy-side state machine. Not exposed as an RPC primitive — the engine doesn't need to know `run_task` exists.

**Primitive:**

- `list_tasks` — reads `Registry::list()`.
- `spawn_task(name, args, timeout?)` → `{task_id}` — backgrounded, returns immediately.
- `kill_task(id, signal)` / `kill_all` / `kill_process(id, signal)` / `quit` — direct engine handle calls.
- `get_graph` — current `GraphSnapshot`.
- `get_task(id)` — single node detail including summary.
- `get_logs(task_id, since_seq?, until_seq?, limit?, filter?)` → `{entries, next_seq, has_more}` — cursor-paged, bounded.
- `grep_logs(task_id, pattern, limit?, scope: descendants|self_only)` → matches.

The `spawn_task` + `get_task` + `get_logs` + `kill_task` set is what makes the long-running interaction loop work: start a service, exercise it via other tools, query its logs, edit code, restart. That loop is the architectural justification for the whole frontend.

## Output access

Two layers, two shapes:

- **MCP surface (pull):** `get_logs` is cursor-paged. Stateless, bounded, fixed memory, agent decides when it has enough. The right shape for tools an LLM is composing — each call is independent.
- **RPC layer (push):** `subscribe_logs` is a streaming primitive on the WebSocket. Used internally by `run_task` to keep a tail buffer warm without polling, and available if a future MCP tool wants push semantics. Filtering happens engine-side in both cases; the same `filter` value type drives both.

`LogStore` already has stable monotonic `seq` values. New helpers needed: range-by-seq and last-N-by-source for `get_logs`, plus a filtered subscription that emits entries as they land for `subscribe_logs`. The existing engine in `src/log/filter.rs` plugs into both.

## Summary slot

A new `Option<String>` field on `TaskExecution`, written by `ctx.summary(s)` (overwrites; last-write-wins). Surfaced on `TaskNode` in graph snapshots and in `get_task`/`run_task` responses. Tasks opt in by calling `ctx.summary(...)` before returning; if they don't, the field is `None` and consumers fall back to `last_n_lines`.

The motivating example: a `cargo build` task post-processes its raw output (errors, warnings, failing crate) and publishes a few lines that an agent can act on without paging through thousands of compiler messages. Pattern works in standalone build tools; should work here.

Single string for v1. No append-stream, no structured fields, no timestamps. Add complexity if a real use case demands it.

## Change detection

The current "shell out to `cargo build` on every invocation" model is acceptable for one-shot CLI use because the human is paying the latency tax of typing a command. MCP changes the contract: a tool call would indirectly pay it, and an agent will hammer this. Cargo's incremental check stat()s the dep graph and parses Cargo.lock — on a clean workspace this is ~150-400ms, dead time per call.

**Decision:** the supervisor owns change detection. It runs a file watcher and only invokes cargo when something semantically changed. Tool calls hit a warm, up-to-date child instantly.

Flow:

```
supervisor startup:
  discover RUNME.rs files
  cargo build (cold)
  spawn child with --ui rpc
  start notify watcher on RUNME.rs files (and sibling .rs files)

on file event:
  debounce 200ms
  rebuild via existing compile.rs path
  if success: kill child, respawn
  if failure: keep old child running, surface error to MCP clients
  re-discover and rebuild watch set if RUNME.rs files appeared/disappeared

on tool call:
  if rebuilding: block until done
  if last-rebuild-failed: error with build output
  else: forward to child
```

**What's watched:**

- Every discovered RUNME.rs.
- Every `.rs` file in each RUNME.rs's directory (a RUNME.rs can `mod foo;` a sibling). Same .gitignore rules as discover.

**What's not watched:**

- Workspace `Cargo.toml` files. Frontmatter `[dependencies]` is the primary dep mechanism; the workspace `Cargo.toml` edge case can be revisited if it bites.
- The generated workspace in the cache dir (we own it).

**The hash-and-skip-cargo optimization** — computing a hash of all RUNME.rs files plus frontmatter and skipping cargo entirely when unchanged — is _not_ in v1. The file watcher already eliminates the per-call cost; the additional optimization is a fixed cost only paid on watcher-triggered rebuilds.

`notify` is the watcher crate. macOS FSEvents coalesces aggressively — 200ms debounce is the right window; shorter sees too many spurious events.

## Build/restart state machine

Three states: `Running`, `Rebuilding`, `Failed`. Tool calls behave per-state:

| State      | Tool call behavior                  |
| ---------- | ----------------------------------- |
| Running    | Forward to child.                   |
| Rebuilding | Block until transition.             |
| Failed     | Error with build stderr in message. |

Transitions:

- `Running` → `Rebuilding` on debounced file event.
- `Rebuilding` → `Running` on successful rebuild + child ready (kill old child, spawn new, wait for connection-info JSON, connect WebSocket).
- `Rebuilding` → `Failed` on cargo failure. Old child stays alive (we never killed it). Tool calls go to error state.
- `Failed` → `Rebuilding` on next file event.
- `Failed` → `Running` if a subsequent rebuild succeeds.

The agent is never disconnected. Their `task_id`s become stale on restart, but agents don't typically hold ids across long gaps in practice; if they do, they get a clean "not found" and re-query.

What survives a restart: nothing. IDs reset, logs gone, in-flight tasks die. Honest, simple. If "logs persist across rebuilds" turns out to matter, it's a future supervisor feature (sqlite mirror, in-memory ring), not a v1 concern.

## What we are explicitly not doing

- No web UI. No second public API. No auth. No semver discipline on the RPC. Add when needed.
- No SSE / HTTP long-poll. WebSocket carries everything.
- No Windows. No Unix-socket fallback.
- No engine state mirrored across restarts.
- No per-task MCP tool generation with synthesized JSON schemas. Generic `spawn_task(name, args: string[])` — agents read `--help` if they need argument detail. JSON-payload args via `serde::Deserialize` is plausible for tasks that want it, but not v1.
- No reconnection logic on the agent side. The MCP proxy↔child WebSocket is loopback; if it drops, something is very wrong.
- No streaming MCP responses. The MCP tool surface is request/response — long-running interaction is `spawn_task` + `get_logs` cursor pulls. (Push exists on the RPC, just not surfaced through MCP.)

## Open / next

- **WebSocket message format.** Concrete JSON shapes for each of the three families (request/response, graph events, log events) and the full call list. Families and discriminator decided; field-level shape still to write down.
- **`LogStore` query helpers.** Range-by-seq and last-N methods for `get_logs`; filtered live subscription for `subscribe_logs`.
- **`ctx.summary(s)` API.** Implementation is small; co-design with MCP.
- **MCP framework.** `rmcp` (official SDK).
- **Implementation order.** Land `--ui rpc` first; prove the protocol works against a websocket client. Then `--ui mcp` with the supervisor + watcher + MCP proxy on top.
