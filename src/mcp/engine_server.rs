//! Headless MCP engine daemon.
//!
//! `runme --engine` spins up a single-tenant TCP server on
//! `127.0.0.1:0`, prints the OS-assigned port as a single JSON line on
//! stdout (`{"port": <u16>}\n`), accepts ONE supervisor connection, and
//! ferries [`WireMessage`] frames between the supervisor and the in-process
//! [`Engine`].
//!
//! See `docs/mcp_design.md` § Topology, § Engine cleanup on disconnect,
//! § RPC protocol, § Method semantics, § Events, § Wire-up.
//!
//! # Lifecycle
//!
//! - The TCP connection is the engine's lifeline. On read-EOF or write
//!   error, the server cancels every running task via
//!   [`EngineHandle::kill_all`], waits up to [`crate::execution::engine::CANCEL_TIMEOUT`]
//!   for clean exits, and then exits the process.
//! - There is no reconnect logic. A new supervisor invocation spawns a
//!   new engine subprocess.
//! - A second incoming connection on the listener is refused (logged to
//!   stderr and dropped).
//!
//! # Concurrency
//!
//! A single `mpsc::UnboundedSender<WireMessage>` is cloned across all
//! producers (request handler, graph forwarder, log subscription
//! forwarders). One writer task drains the channel and owns the
//! transport's write half, so we never need a mutex on the wire.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::execution::{Engine, EngineHandle};
use crate::task::Registry;
use crate::log::LogEntry;
use crate::mcp::transport::WireTransport;
use crate::mcp::wire::{
    Event, GrepScope, Request, Response, RpcError, SubscriptionId, WireMessage,
};

/// Maximum graceful-shutdown wait for tasks to exit on disconnect.
///
/// Mirrors `execution::engine::CANCEL_TIMEOUT` — the cancel ladder
/// inside `kill_all` already enforces this per-task; we wait an extra
/// budget here in case `kill_all` itself is slow to drain.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Bind on `127.0.0.1:0`, print the port on stdout, and serve one
/// supervisor connection. Blocks until the connection drops or a fatal
/// error occurs, then `std::process::exit`s.
pub async fn run(registry: Arc<Registry>, start_task_id: u64) -> ! {
    let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("rnme --engine: failed to bind 127.0.0.1:0: {e}");
            std::process::exit(1);
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!("rnme --engine: local_addr failed: {e}");
            std::process::exit(1);
        }
    };

    // Print the port line — the ONLY thing we ever write to stdout.
    let line = serde_json::to_string(&serde_json::json!({"port": port}))
        .expect("serializing {port} cannot fail");
    println!("{line}");
    let _ = std::io::stdout().flush();

    let exit_code = serve_on(listener, registry, start_task_id).await;
    std::process::exit(exit_code);
}

/// Serve on a pre-bound listener. Used by tests that need to inject
/// their own listener (and thus their own port).
///
/// Returns the process exit code: `0` on a clean shutdown, `1` if
/// something went wrong while accepting or while supervising.
pub async fn serve_on(
    listener: TcpListener,
    registry: Arc<Registry>,
    start_task_id: u64,
) -> i32 {
    // Construct the engine. `start_task_id` is process-global state; the
    // Engine constructor seeds it before any TaskExecution mints an id.
    let (engine, handle) = Engine::start_with_task_id_offset(registry, start_task_id);

    // Accept a single connection. Anything beyond that is refused with
    // a stderr log (per design § Topology, supervisor↔engine is 1:1).
    let stream = match listener.accept().await {
        Ok((s, _)) => s,
        Err(e) => {
            eprintln!("rnme --engine: accept failed: {e}");
            engine.shutdown().await;
            return 1;
        }
    };

    // Drop subsequent accepts in the background so they get a clean RST
    // rather than hanging.
    let listener = Arc::new(listener);
    let refuser = listener.clone();
    let refuser_task = tokio::spawn(async move {
        loop {
            match refuser.accept().await {
                Ok((s, addr)) => {
                    eprintln!(
                        "rnme --engine: refusing extra connection from {addr} (engine is single-tenant)"
                    );
                    drop(s);
                }
                Err(e) => {
                    eprintln!("rnme --engine: extra-accept loop error: {e}");
                    break;
                }
            }
        }
    });

    let exit_code = serve_connection(stream, handle.clone()).await;

    // Cleanup: cancel every task and let the engine drain.
    let _ = handle.kill_all().await;
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, handle.quit()).await;
    refuser_task.abort();
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, engine.shutdown()).await;

    exit_code
}

// ---------------------------------------------------------------------------
// Connection-scoped state
// ---------------------------------------------------------------------------

/// Per-connection live log subscription.
///
/// Holds the abort handle for the forwarder task that pulls entries
/// off `LogStore::subscribe_with` and pushes them onto the writer
/// channel as `Event::Log` frames. Aborting the handle stops
/// forwarding and lets the underlying stream drop.
struct SubscriptionState {
    forwarder: JoinHandle<()>,
}

impl Drop for SubscriptionState {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

/// Connection-local subscription registry.
type Subscriptions = std::sync::Mutex<HashMap<SubscriptionId, SubscriptionState>>;

/// Drive a single accepted connection. Returns the desired process
/// exit code.
async fn serve_connection(stream: TcpStream, handle: EngineHandle) -> i32 {
    // Wrap the stream in the wire transport, then split so the reader
    // and writer run on separate tasks without sharing a mutex.
    let transport = WireTransport::new(stream);
    let (mut writer, mut reader) = transport.into_split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WireMessage>();

    // Writer task: drain the mpsc, send each WireMessage on the wire.
    // Exits on channel close or transport error; either way, we want
    // the connection to wind down.
    let writer_task: JoinHandle<()> = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = writer.send(&msg).await {
                eprintln!("rnme --engine: write error: {e}");
                break;
            }
        }
    });

    // Graph forwarder: every snapshot change emits `Event::Graph`.
    let graph_tx = out_tx.clone();
    let mut graph_rx = handle.graph.clone();
    let graph_task: JoinHandle<()> = tokio::spawn(async move {
        // Send the current snapshot immediately so the supervisor has
        // a baseline before any other events.
        let initial = graph_rx.borrow().clone();
        if graph_tx
            .send(WireMessage::Event(Event::Graph { snapshot: initial }))
            .is_err()
        {
            return;
        }
        while graph_rx.changed().await.is_ok() {
            let snap = graph_rx.borrow().clone();
            if graph_tx
                .send(WireMessage::Event(Event::Graph { snapshot: snap }))
                .is_err()
            {
                break;
            }
        }
    });

    let next_subscription = Arc::new(AtomicU64::new(1));
    let subscriptions: Arc<Subscriptions> = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Request loop. On EOF / transport error, exit and let the cleanup
    // path (in `serve_on`) tear down the engine.
    let mut clean_close = true;
    loop {
        match reader.recv().await {
            Ok(WireMessage::Request { id, body }) => {
                let handle = handle.clone();
                let out_tx = out_tx.clone();
                let next_sub = next_subscription.clone();
                let subs = subscriptions.clone();
                // Each request handler is spawned so a slow task list
                // doesn't head-of-line block log subscription replies.
                tokio::spawn(async move {
                    let response = handle_request(handle, body, out_tx.clone(), next_sub, subs).await;
                    let _ = out_tx.send(WireMessage::Response { id, body: response });
                });
            }
            Ok(WireMessage::Response { .. }) | Ok(WireMessage::Event(_)) => {
                // Engine is the server; we never expect responses or
                // events from the supervisor. Treat as protocol error
                // but don't kill the connection over it.
                eprintln!(
                    "rnme --engine: unexpected non-request message from supervisor; ignoring"
                );
            }
            Err(crate::mcp::transport::TransportError::Closed) => {
                // Clean disconnect.
                break;
            }
            Err(e) => {
                eprintln!("rnme --engine: read error: {e}");
                clean_close = false;
                break;
            }
        }
    }

    // Drop the request-side sender so the writer task drains and exits.
    drop(out_tx);
    graph_task.abort();
    // Wait briefly for the writer to flush. If it stalls we abort.
    let _ = tokio::time::timeout(Duration::from_millis(500), writer_task).await;
    // Drop subscriptions to abort their forwarders.
    drop(subscriptions);

    if clean_close { 0 } else { 1 }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

async fn handle_request(
    handle: EngineHandle,
    req: Request,
    out_tx: mpsc::UnboundedSender<WireMessage>,
    next_sub: Arc<AtomicU64>,
    subs: Arc<Subscriptions>,
) -> Result<Response, RpcError> {
    match req {
        Request::ListTasks => Ok(Response::ListTasks(handle.registry.list_info())),

        Request::SpawnTask { name, args, opts } => {
            let def = handle
                .registry
                .resolve(&name)
                .map_err(|e| RpcError::NotFound(e.to_string()))?;

            // Snapshot the seq counter BEFORE we spawn so the supervisor
            // can pass it as `from_seq` on a follow-up SubscribeLogs and
            // never miss an entry (and never re-receive earlier ones).
            let initial_seq = {
                let store = handle.log_store.lock().await;
                store.seq_gen().current()
            };

            let mut builder = handle.spawn_task(def, args);
            if let Some(d) = opts.timeout {
                builder = builder.timeout(d);
            }
            let task_id = builder.await.map_err(RpcError::Engine)?;
            Ok(Response::SpawnTask {
                task_id,
                initial_seq,
            })
        }

        Request::KillTask { task_id, signal } => {
            handle
                .kill_task(task_id, signal)
                .await
                .map_err(RpcError::Engine)?;
            Ok(Response::KillTask)
        }

        Request::KillProcess { process_id, signal } => {
            handle
                .kill_process(process_id, signal)
                .await
                .map_err(RpcError::Engine)?;
            Ok(Response::KillProcess)
        }

        Request::KillAll => {
            handle.kill_all().await.map_err(RpcError::Engine)?;
            Ok(Response::KillAll)
        }

        Request::GetLogs {
            task_id,
            since_seq,
            until_seq,
            limit,
            filter,
        } => {
            let sources = handle.source_ids_for(task_id);
            if sources.is_empty() {
                return Err(RpcError::NotFound(format!(
                    "task {task_id} not in graph"
                )));
            }

            let parsed_filter = match filter.as_deref() {
                Some(f) => Some(
                    crate::log::filter::parse(f).map_err(RpcError::FilterParse)?,
                ),
                None => None,
            };

            let limit = clamp_limit(limit);
            let (entries, next_seq, has_more) = {
                let store = handle.log_store.lock().await;
                match parsed_filter {
                    Some(expr) => {
                        let pred = move |e: &LogEntry| crate::log::filter::matches(&expr, e);
                        store.get_range(
                            &sources,
                            since_seq.unwrap_or(0),
                            until_seq.unwrap_or(u64::MAX),
                            limit,
                            Some(&pred),
                        )
                    }
                    None => store.get_range(
                        &sources,
                        since_seq.unwrap_or(0),
                        until_seq.unwrap_or(u64::MAX),
                        limit,
                        None,
                    ),
                }
            };
            Ok(Response::GetLogs {
                entries,
                next_seq,
                has_more,
            })
        }

        Request::GrepLogs {
            task_id,
            pattern,
            limit,
            scope,
        } => {
            let regex = regex::Regex::new(&pattern).map_err(|e| RpcError::BadRequest(e.to_string()))?;
            let sources = match scope {
                GrepScope::Descendants => handle.source_ids_for(task_id),
                GrepScope::SelfOnly => vec![task_id],
            };
            if sources.is_empty() {
                return Err(RpcError::NotFound(format!(
                    "task {task_id} not in graph"
                )));
            }
            let limit = clamp_limit(limit);
            let matches = {
                let store = handle.log_store.lock().await;
                store.grep(&sources, &regex, limit)
            };
            Ok(Response::GrepLogs { matches })
        }

        Request::SubscribeLogs {
            task_id,
            filter,
            from_seq,
        } => {
            let sources = handle.source_ids_for(task_id);
            if sources.is_empty() {
                return Err(RpcError::NotFound(format!(
                    "task {task_id} not in graph"
                )));
            }

            // Build the predicate. `parse` returns String on error → FilterParse.
            let pred: Arc<dyn Fn(&LogEntry) -> bool + Send + Sync> = match filter {
                Some(s) => {
                    let expr =
                        crate::log::filter::parse(&s).map_err(RpcError::FilterParse)?;
                    Arc::new(move |e: &LogEntry| crate::log::filter::matches(&expr, e))
                }
                None => Arc::new(|_: &LogEntry| true),
            };

            // Allocate a subscription id and start the forwarder.
            let sub_id = SubscriptionId(next_sub.fetch_add(1, Ordering::Relaxed));

            // `subscribe_with` requires &mut LogStore.
            let stream = {
                let mut store = handle.log_store.lock().await;
                let pred = pred.clone();
                store.subscribe_with(
                    &sources,
                    move |e: &LogEntry| pred(e),
                    from_seq.unwrap_or(0),
                )
            };

            let out_tx = out_tx.clone();
            let forwarder: JoinHandle<()> = tokio::spawn(async move {
                use futures::StreamExt;
                let mut stream = Box::pin(stream);
                while let Some(entry) = stream.next().await {
                    if out_tx
                        .send(WireMessage::Event(Event::Log {
                            subscription_id: sub_id,
                            entry,
                        }))
                        .is_err()
                    {
                        // Connection going away.
                        break;
                    }
                }
            });

            subs.lock()
                .expect("subscriptions mutex poisoned")
                .insert(sub_id, SubscriptionState { forwarder });

            Ok(Response::SubscribeLogs {
                subscription_id: sub_id,
            })
        }

        Request::UnsubscribeLogs { subscription_id } => {
            let removed = subs
                .lock()
                .expect("subscriptions mutex poisoned")
                .remove(&subscription_id);
            match removed {
                Some(_) => Ok(Response::UnsubscribeLogs),
                None => Err(RpcError::NotFound(format!(
                    "subscription {} not found",
                    subscription_id.0
                ))),
            }
        }

        Request::CountLogs { task_id } => {
            let all_sources = handle.source_ids_for(task_id);
            if all_sources.is_empty() {
                return Err(RpcError::NotFound(format!(
                    "task {task_id} not in graph"
                )));
            }
            let task_sources = handle.task_only_source_ids(task_id);
            let (stdout, stderr, events) = {
                let store = handle.log_store.lock().await;
                store.count_by_stream(&all_sources, &task_sources)
            };
            Ok(Response::CountLogs(crate::mcp::wire::LogCounts {
                stdout,
                stderr,
                events,
            }))
        }
    }
}

/// Clamp `limit` to a sane window: default 200, max 5000.
fn clamp_limit(limit: Option<u32>) -> usize {
    const DEFAULT: usize = 200;
    const MAX: usize = 5000;
    match limit {
        None => DEFAULT,
        Some(n) => (n as usize).min(MAX),
    }
}
