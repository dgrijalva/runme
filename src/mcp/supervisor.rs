//! Supervisor core for the MCP frontend.
//!
//! The supervisor runs in the *outer* `rnme` process when the user invokes
//! `rnme --mcp`. It owns one or more child engine generations
//! (`rnme --engine` subprocesses), demuxes wire protocol traffic over each
//! generation's TCP connection, and exposes a small async API the Phase 6
//! tool surface will plug into.
//!
//! # Generation lifecycle
//!
//! - Generations are spawned by [`Supervisor::spawn_initial_generation`]
//!   (Phase 4) or [`Supervisor::rotate_latest`] (stub for Phase 5).
//! - The first gen always becomes the latest. New top-level spawns route
//!   to the latest gen.
//! - Generations whose tasks have all completed STAY ALIVE; their
//!   `LogStore` remains queryable for the lifetime of the supervisor.
//! - The single exception: a generation that NEVER had a task spawned
//!   against it before the next gen replaces it retires immediately
//!   (no logs of value).
//! - Cleanup happens on supervisor drop / `shutdown` — closing the writer
//!   channel drops the TCP write half; the engine sees EOF and cleans
//!   itself up via the Phase 1 disconnect path.
//!
//! # No file watcher / build state machine here
//!
//! Phase 5 (`i-build-state`) will drive `rotate_latest` from a debounced
//! file-watcher and add the `BuildState` machine. This slice ships only
//! the supervisor primitives.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::execution::{GraphSnapshot, KillSignal, SpawnOptions, TaskId};
use crate::mcp::build::{
    BuildState, BuildStatusInfo, FAILURE_HEAD_LINES, WatchSet, WatchSetSetup, head_of_failure,
};
use crate::mcp::routing::{
    Address, AddressError, EngineMap, GenerationId, RewrittenSnapshot, ResolveError,
    merge_snapshots, rewrite_snapshot,
};
use crate::mcp::transport::{TransportError, WireSink, WireStream, WireTransport};
use crate::mcp::wire::{
    CorrelationId, Event, Request, Response, RpcError, WireMessage,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Outer-driver entry. Initializes stderr-only tracing, builds the
/// supervisor, brings up the file watcher, holds it open until stdin
/// closes (driving any debounced rebuild signals along the way), then
/// shuts down.
///
/// Phase 6 will replace the stdin-EOF wait with the rmcp service loop;
/// the rebuild-signal arm of the select! moves into the same task.
pub async fn run() {
    install_stderr_tracing();

    let mut supervisor = match Supervisor::new().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("supervisor init failed: {e}");
            std::process::exit(1);
        }
    };

    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        tracing::error!("supervisor: could not determine current dir: {e}");
        std::process::exit(1);
    });
    if let Err(e) = supervisor.start_watcher(cwd) {
        tracing::warn!("supervisor: file watcher failed to start: {e}");
    }

    // Drive: rebuild signals fire `handle_rebuild_signal`; stdin EOF
    // breaks the loop and triggers shutdown. We `take()` the receiver
    // so we own it locally and don't fight `&mut self`.
    let mut rebuild_rx = supervisor.take_rebuild_rx();
    let mut stdin_buf = Vec::new();
    let mut stdin = tokio::io::stdin();

    loop {
        tokio::select! {
            biased;
            sig = async {
                match rebuild_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match sig {
                    Some(()) => {
                        if let Err(e) = supervisor.handle_rebuild_signal().await {
                            tracing::error!("supervisor: rebuild driver error: {e}");
                        }
                    }
                    None => {
                        // Channel closed → watcher is gone; stop polling.
                        rebuild_rx = None;
                    }
                }
            }
            res = stdin.read_to_end(&mut stdin_buf) => {
                let _ = res;
                break;
            }
        }
    }

    supervisor.shutdown().await;
}

/// Install a stderr-only `tracing-subscriber`. Required because rmcp
/// (Phase 6) owns stdout — any stdout writes corrupt JSON-RPC framing.
fn install_stderr_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{EnvFilter, Layer};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,rnme=info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter);
    let subscriber = tracing_subscriber::registry().with(fmt_layer);
    let _ = tracing::subscriber::set_global_default(subscriber);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during supervisor setup.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("spawn engine: {0}")]
    Spawn(#[from] SpawnError),
    #[error("connect engine: {0}")]
    Connect(#[from] std::io::Error),
}

/// Errors that can occur while spawning a child engine.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("engine child has no stdout")]
    NoStdout,
    #[error("engine exited before printing port; stderr: {0}")]
    EngineExited(String),
    #[error("malformed port line: {0}")]
    BadPortLine(String),
}

// ---------------------------------------------------------------------------
// Spawner trait + implementations
// ---------------------------------------------------------------------------

/// Type-erased drop-guard for a spawned engine. Holds whatever resource
/// the spawner needs to keep alive (a `Child`, a `JoinHandle`, etc.).
/// The supervisor never inspects it; dropping it must clean up the
/// engine's process / task.
pub type EngineGuard = Box<dyn std::any::Any + Send + Sync>;

/// A spawned engine: the port it's listening on plus a guard whose drop
/// terminates the engine.
pub struct SpawnedEngine {
    pub port: u16,
    pub guard: EngineGuard,
}

/// Pluggable engine launcher. Production code uses
/// [`ProcessEngineSpawner`]; tests use an in-process variant that calls
/// `engine_server::serve_on` directly.
///
/// Returns a boxed future to keep the trait object-safe across Rust
/// editions without depending on `async_trait`.
pub trait EngineSpawner: Send + Sync {
    fn spawn<'a>(
        &'a self,
        start_task_id: u64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<SpawnedEngine, SpawnError>> + Send + 'a>>;
}

/// Production spawner: re-execs `current_exe()` with `--engine`, parses
/// the `{"port": N}` line from stdout.
///
/// Per G0: when the supervisor is running, `current_exe()` is the outer
/// `rnme` binary. Re-entering it with `--engine` runs the outer driver,
/// which transparently does discover+compile+exec into a runner-with-
/// `--engine` (the engine daemon).
#[derive(Default)]
pub struct ProcessEngineSpawner;

impl EngineSpawner for ProcessEngineSpawner {
    fn spawn<'a>(
        &'a self,
        start_task_id: u64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<SpawnedEngine, SpawnError>> + Send + 'a>>
    {
        Box::pin(async move {
            let exe = std::env::current_exe()?;
            let mut child = tokio::process::Command::new(&exe)
                .arg("--engine")
                .arg("--start-task-id")
                .arg(start_task_id.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()?;

            let stdout = child.stdout.take().ok_or(SpawnError::NoStdout)?;
            let mut lines = BufReader::new(stdout).lines();
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) | Err(_) => {
                    // Child exited before printing port line. Drain
                    // stderr so the caller can see why.
                    let mut buf = Vec::new();
                    if let Some(mut s) = child.stderr.take() {
                        let _ = s.read_to_end(&mut buf).await;
                    }
                    return Err(SpawnError::EngineExited(
                        String::from_utf8_lossy(&buf).into_owned(),
                    ));
                }
            };

            let parsed: serde_json::Value = serde_json::from_str(&line)
                .map_err(|e| SpawnError::BadPortLine(format!("{e}: {line}")))?;
            let port = parsed
                .get("port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| SpawnError::BadPortLine(line.clone()))?
                as u16;

            Ok(SpawnedEngine {
                port,
                guard: Box::new(child),
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// Map of in-flight request correlation ids → response oneshot senders.
type InFlight = Arc<Mutex<HashMap<CorrelationId, oneshot::Sender<Result<Response, RpcError>>>>>;

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Per-generation state owned by the supervisor.
struct Generation {
    #[allow(dead_code)]
    id: GenerationId,
    /// Send half: every outbound `WireMessage` goes through here. The
    /// writer task drains the channel and writes to the TCP write half.
    writer_tx: mpsc::UnboundedSender<WireMessage>,
    /// Outstanding requests waiting on responses.
    in_flight: InFlight,
    /// Latest cached graph snapshot for this gen.
    latest_snapshot: Arc<Mutex<Option<GraphSnapshot>>>,
    /// Reader task; aborted on retire / shutdown.
    reader_handle: JoinHandle<()>,
    /// Writer task; finishes when the channel closes.
    writer_handle: JoinHandle<()>,
    /// Drop guard for the spawned engine (a `Child` for production, an
    /// abort-on-drop `JoinHandle` for in-process tests).
    _guard: EngineGuard,
    /// Has any top-task been registered against this gen? Drives the
    /// "never had tasks → immediate retire" rule.
    has_had_tasks: AtomicBool,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Supervises one or more child engine generations.
pub struct Supervisor {
    gens: HashMap<GenerationId, Generation>,
    latest_gen: Option<GenerationId>,
    engine_map: EngineMap<GenerationId>,
    next_correlation: AtomicU64,
    next_gen_id: AtomicU64,
    next_start_task_id: AtomicU64,
    spawner: Box<dyn EngineSpawner>,
    /// Tri-state build state machine. Read by spawn-shaped tools through
    /// [`Supervisor::check_can_spawn`] and exposed read-only via
    /// [`Supervisor::build_status`].
    build_state: Arc<Mutex<BuildState>>,
    /// Signaled on every build-state transition. Spawn-shaped tools that
    /// observed `Rebuilding` re-check after this fires.
    build_state_changed: Arc<Notify>,
    /// File watcher set up by [`Supervisor::start_watcher`]. `None` for
    /// tests / contexts that don't want a live watcher.
    watch_set: Option<WatchSet>,
    /// Receiver feeding rebuild signals into [`Supervisor::handle_rebuild_signal`].
    /// Owned by the supervisor between `start_watcher` and the first
    /// driver-loop iteration; tests can take it via [`Supervisor::take_rebuild_rx`].
    rebuild_rx: Option<mpsc::Receiver<()>>,
}

/// Each generation gets a disjoint id range so engines never collide on
/// top-task ids. 1M ids per gen is far more than any reasonable session.
const ID_RANGE_PER_GEN: u64 = 1_000_000;

impl Supervisor {
    /// Build a supervisor with the production process spawner and bring
    /// the initial generation up.
    pub async fn new() -> Result<Self, SupervisorError> {
        Self::new_with_spawner(Box::new(ProcessEngineSpawner)).await
    }

    /// Build a supervisor with a custom spawner. Used by tests to inject
    /// in-process engines.
    ///
    /// Does NOT start the file watcher — call [`Supervisor::start_watcher`]
    /// separately. Tests that don't want a watcher skip that step.
    pub async fn new_with_spawner(
        spawner: Box<dyn EngineSpawner>,
    ) -> Result<Self, SupervisorError> {
        let mut s = Supervisor {
            gens: HashMap::new(),
            latest_gen: None,
            engine_map: EngineMap::new(),
            next_correlation: AtomicU64::new(0),
            next_gen_id: AtomicU64::new(0),
            next_start_task_id: AtomicU64::new(1),
            spawner,
            build_state: Arc::new(Mutex::new(BuildState::Idle)),
            build_state_changed: Arc::new(Notify::new()),
            watch_set: None,
            rebuild_rx: None,
        };
        s.spawn_initial_generation().await?;
        Ok(s)
    }

    /// Bring up the file watcher rooted at `cwd`. Discovers RUNME.rs
    /// files, subscribes the live `notify::RecommendedWatcher` to each
    /// parent dir non-recursively, and stages the rebuild-signal
    /// receiver for [`Supervisor::handle_rebuild_signal`].
    ///
    /// Returns `Err` if `notify::RecommendedWatcher::new` itself fails
    /// (rare — almost always an OS-level resource limit).
    pub fn start_watcher(&mut self, cwd: PathBuf) -> notify::Result<()> {
        let WatchSetSetup {
            watch_set,
            rebuild_rx,
        } = WatchSet::build(cwd)?;
        self.watch_set = Some(watch_set);
        self.rebuild_rx = Some(rebuild_rx);
        Ok(())
    }

    /// Take the rebuild receiver out of the supervisor. Used by callers
    /// (and tests) that want to drive the receive themselves rather than
    /// relying on [`Supervisor::handle_rebuild_signal`] in a hot loop.
    pub fn take_rebuild_rx(&mut self) -> Option<mpsc::Receiver<()>> {
        self.rebuild_rx.take()
    }

    /// Handle to the current build state for read-only consumers.
    pub fn build_state_handle(&self) -> Arc<Mutex<BuildState>> {
        Arc::clone(&self.build_state)
    }

    /// Notification handle for build-state transitions.
    pub fn build_state_changed_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.build_state_changed)
    }

    /// Test-only: clone of the rebuild signal sender so tests can fire
    /// rebuilds without relying on a live notify watcher.
    pub fn rebuild_signal_tx(&self) -> Option<mpsc::Sender<()>> {
        self.watch_set.as_ref().map(|w| w.rebuild_tx())
    }

    fn alloc_gen_id(&self) -> GenerationId {
        GenerationId(self.next_gen_id.fetch_add(1, Ordering::Relaxed))
    }

    fn alloc_start_task_id(&self) -> u64 {
        self.next_start_task_id
            .fetch_add(ID_RANGE_PER_GEN, Ordering::Relaxed)
    }

    /// Spawn the very first generation and make it the latest.
    async fn spawn_initial_generation(&mut self) -> Result<(), SupervisorError> {
        let gen_id = self.alloc_gen_id();
        let start_task_id = self.alloc_start_task_id();
        let spawned = self.spawner.spawn(start_task_id).await?;
        let g = self.connect_gen(gen_id, spawned).await?;
        self.gens.insert(gen_id, g);
        self.latest_gen = Some(gen_id);
        Ok(())
    }

    /// Spawn a new generation, make it the latest, and retire any prior
    /// latest generation that never had a task spawned.
    ///
    /// Called by the file-watcher driver loop after a debounced edit. The
    /// build-state transitions are managed by
    /// [`Supervisor::handle_rebuild_signal`]; calling `rotate_latest`
    /// directly skips them (used by some tests for the never-had-tasks
    /// retirement check).
    pub async fn rotate_latest(&mut self) -> Result<(), SupervisorError> {
        let prior_latest = self.latest_gen;

        let gen_id = self.alloc_gen_id();
        let start_task_id = self.alloc_start_task_id();
        let spawned = self.spawner.spawn(start_task_id).await?;
        let g = self.connect_gen(gen_id, spawned).await?;
        self.gens.insert(gen_id, g);
        self.latest_gen = Some(gen_id);

        if let Some(prev) = prior_latest
            && let Some(prev_gen) = self.gens.get(&prev)
            && !prev_gen.has_had_tasks.load(Ordering::Relaxed)
        {
            // Never-had-tasks generation: retire immediately.
            self.retire_gen(prev).await;
        }
        Ok(())
    }

    /// Drive a single rebuild cycle: transition `BuildState` to
    /// `Rebuilding`, call [`Supervisor::rotate_latest`], then transition
    /// to `Idle` (success) or `LastBuildFailed` (engine spawn failed).
    ///
    /// Returns `Ok(())` for both successful rebuilds and recoverable
    /// build failures — the failure is captured into `BuildState` and
    /// surfaced to agents via [`Supervisor::check_can_spawn`] /
    /// [`Supervisor::build_status`]. Only unrecoverable supervisor
    /// errors (transport, IO) propagate as `Err`.
    ///
    /// # Refresh
    ///
    /// On success, the [`WatchSet`] (if any) is refreshed so newly
    /// added / removed RUNME.rs files take effect immediately.
    pub async fn handle_rebuild_signal(&mut self) -> Result<(), SupervisorError> {
        // Transition → Rebuilding and notify any waiters.
        {
            let mut s = self.build_state.lock().await;
            *s = BuildState::Rebuilding;
        }
        self.build_state_changed.notify_waiters();

        let outcome = self.rotate_latest().await;

        match outcome {
            Ok(()) => {
                {
                    let mut s = self.build_state.lock().await;
                    *s = BuildState::Idle;
                }
                self.build_state_changed.notify_waiters();
                if let Some(ws) = self.watch_set.as_mut() {
                    ws.refresh().await;
                }
                Ok(())
            }
            Err(SupervisorError::Spawn(SpawnError::EngineExited(stderr))) => {
                {
                    let mut s = self.build_state.lock().await;
                    *s = BuildState::LastBuildFailed {
                        last_failure_output: stderr,
                    };
                }
                self.build_state_changed.notify_waiters();
                // Build failed but the supervisor is otherwise healthy
                // — existing live gens keep serving existing-state tools.
                Ok(())
            }
            Err(other) => {
                tracing::error!("rotate_latest failed: {}", other);
                // Reset to Idle so spawn-shaped tools can keep trying;
                // the underlying error has been logged.
                {
                    let mut s = self.build_state.lock().await;
                    *s = BuildState::Idle;
                }
                self.build_state_changed.notify_waiters();
                Err(other)
            }
        }
    }

    /// Spawn-shaped tool guard. Phase 6's `spawn_task` / `list_tasks` /
    /// `run_task` call this before forwarding to the latest gen.
    ///
    /// - `Idle` → `Ok(())`, proceed.
    /// - `Rebuilding` → wait on `build_state_changed`, then re-check
    ///   (loops until the state leaves `Rebuilding`).
    /// - `LastBuildFailed` → `Err(RpcError::BadRequest)` carrying the
    ///   head of the captured stderr; agents fetch the full output via
    ///   `get_build_status`.
    ///
    /// Existing-state tools (`kill_task`, `get_logs`, etc.) bypass this
    /// guard — they route through `request_addr` and don't care about
    /// the build state.
    pub async fn check_can_spawn(&self) -> Result<(), RpcError> {
        loop {
            // Park a notification BEFORE reading state to avoid the
            // missed-wake race: if the state changes between our read
            // and our `notified().await`, the Notify will already be
            // armed and the await returns immediately.
            let notified = self.build_state_changed.notified();
            tokio::pin!(notified);

            let state = self.build_state.lock().await.clone();
            match state {
                BuildState::Idle => return Ok(()),
                BuildState::LastBuildFailed {
                    last_failure_output,
                } => {
                    let head = head_of_failure(&last_failure_output, FAILURE_HEAD_LINES);
                    return Err(RpcError::BadRequest(format!(
                        "build failed; call get_build_status for full output\n{head}"
                    )));
                }
                BuildState::Rebuilding => {
                    // Drop the lock before parking. The notification
                    // we registered above remains armed.
                    notified.await;
                }
            }
        }
    }

    /// Read-only inspection for the `get_build_status` MCP tool.
    pub async fn build_status(&self) -> BuildStatusInfo {
        let s = self.build_state.lock().await;
        BuildStatusInfo::from(&*s)
    }

    /// Build per-gen state from a freshly-spawned engine: connect TCP,
    /// split the transport, spawn reader + writer tasks.
    async fn connect_gen(
        &self,
        gen_id: GenerationId,
        spawned: SpawnedEngine,
    ) -> Result<Generation, SupervisorError> {
        let stream = TcpStream::connect(("127.0.0.1", spawned.port)).await?;
        let transport = WireTransport::new(stream);
        let (sink, stream_) = transport.into_split();

        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<WireMessage>();
        let in_flight: InFlight = Arc::new(Mutex::new(HashMap::new()));
        let latest_snapshot = Arc::new(Mutex::new(None));

        let writer_handle = spawn_writer(sink, writer_rx);
        let reader_handle =
            spawn_reader(stream_, Arc::clone(&in_flight), Arc::clone(&latest_snapshot));

        Ok(Generation {
            id: gen_id,
            writer_tx,
            in_flight,
            latest_snapshot,
            reader_handle,
            writer_handle,
            _guard: spawned.guard,
            has_had_tasks: AtomicBool::new(false),
        })
    }

    /// Retire a generation: close the writer channel, abort the reader,
    /// drop the engine guard, and remove from `engine_map`.
    async fn retire_gen(&mut self, gen_id: GenerationId) {
        if let Some(g) = self.gens.remove(&gen_id) {
            // Closing writer_tx → writer task drains and exits → engine
            // sees EOF on its read side → engine cleans up.
            drop(g.writer_tx);
            g.reader_handle.abort();
            g.writer_handle.abort();
            // Drain any in-flight oneshots so callers waking up don't
            // hang. Sender drop → recv returns Err.
            let mut map = g.in_flight.lock().await;
            map.clear();
            self.engine_map.retire_generation(gen_id);
            // _guard drops here, killing the engine process / task.
        }
    }

    /// Allocate a fresh `CorrelationId`.
    fn alloc_correlation(&self) -> CorrelationId {
        CorrelationId(self.next_correlation.fetch_add(1, Ordering::Relaxed))
    }

    /// Forward a request to a specific gen and await its response.
    async fn request_gen(
        &self,
        gen_id: GenerationId,
        body: Request,
    ) -> Result<Response, RpcError> {
        let g = self
            .gens
            .get(&gen_id)
            .ok_or_else(|| RpcError::NotFound(format!("generation {} not live", gen_id.0)))?;

        let id = self.alloc_correlation();
        let (tx, rx) = oneshot::channel();
        g.in_flight.lock().await.insert(id, tx);

        if g.writer_tx
            .send(WireMessage::Request { id, body })
            .is_err()
        {
            // Writer channel closed → connection dead.
            g.in_flight.lock().await.remove(&id);
            return Err(RpcError::Internal(format!(
                "generation {} connection closed",
                gen_id.0
            )));
        }

        match rx.await {
            Ok(resp) => resp,
            Err(_) => Err(RpcError::Internal(format!(
                "generation {} dropped request before responding",
                gen_id.0
            ))),
        }
    }

    /// Address-routed request. Parses the dotted address, looks up the
    /// owning gen, and forwards. The closure builds the engine-side
    /// `Request` from the resolved [`Address`] (which carries the
    /// engine-internal `task` id rather than the dotted form).
    pub async fn request_addr(
        &self,
        address: &str,
        body_factory: impl FnOnce(Address) -> Request,
    ) -> Result<Response, RpcError> {
        let addr = address.parse::<Address>().map_err(|e: AddressError| {
            RpcError::BadRequest(e.to_string())
        })?;
        let gen_ref = self
            .engine_map
            .lookup(addr.top)
            .ok_or_else(|| RpcError::NotFound(format!("top-task {} not live", addr.top)))?;
        let gen_id = gen_ref.gen_id;
        let body = body_factory(addr);
        self.request_gen(gen_id, body).await
    }

    /// Spawn a top-level task on the latest gen.
    ///
    /// Returns the dotted address of the new top-level task and the
    /// engine's `initial_seq` (so the caller can pass it as `from_seq`
    /// on a follow-up `SubscribeLogs` and avoid the spawn-then-subscribe
    /// race).
    pub async fn spawn_task(
        &mut self,
        name: String,
        args: Vec<String>,
        opts: SpawnOptions,
    ) -> Result<(String, u64), RpcError> {
        let gen_id = self.latest_gen.ok_or_else(|| {
            RpcError::Internal("no live generation to spawn into".to_string())
        })?;

        let resp = self
            .request_gen(
                gen_id,
                Request::SpawnTask {
                    name,
                    args,
                    opts,
                },
            )
            .await?;

        let (task_id, initial_seq) = match resp {
            Response::SpawnTask {
                task_id,
                initial_seq,
            } => (task_id, initial_seq),
            other => {
                return Err(RpcError::Internal(format!(
                    "unexpected response to SpawnTask: {other:?}"
                )));
            }
        };

        // Register top-task → gen mapping and mark the gen as "has had
        // tasks" so the never-had-tasks retirement rule won't fire on
        // this gen at the next rotate.
        self.engine_map.insert(task_id.0, gen_id, gen_id);
        if let Some(g) = self.gens.get(&gen_id) {
            g.has_had_tasks.store(true, Ordering::Relaxed);
        }

        let dotted = Address::render_task(task_id.0, task_id.0);
        Ok((dotted, initial_seq))
    }

    /// Convenience: kill a task by dotted address.
    pub async fn kill_task(&self, address: &str, signal: KillSignal) -> Result<(), RpcError> {
        let resp = self
            .request_addr(address, |addr| Request::KillTask {
                task_id: TaskId(addr.task),
                signal,
            })
            .await?;
        match resp {
            Response::KillTask => Ok(()),
            other => Err(RpcError::Internal(format!(
                "unexpected response to KillTask: {other:?}"
            ))),
        }
    }

    /// Merged graph snapshot across every live gen, with engine-internal
    /// ids rewritten to dotted addresses. Top tasks ordered by ascending
    /// top-task id.
    pub async fn graph(&self) -> RewrittenSnapshot {
        let mut per_gen = Vec::with_capacity(self.gens.len());
        for (gen_id, g) in &self.gens {
            let snap_opt = g.latest_snapshot.lock().await.clone();
            if let Some(snap) = snap_opt {
                per_gen.push(rewrite_snapshot(&snap, *gen_id));
            }
        }
        merge_snapshots(per_gen)
    }

    /// Cleanly shut down: retire every gen so engines see EOF and exit.
    pub async fn shutdown(&mut self) {
        let ids: Vec<GenerationId> = self.gens.keys().copied().collect();
        for id in ids {
            self.retire_gen(id).await;
        }
        self.latest_gen = None;
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Best-effort cleanup if the caller forgets to `shutdown().await`:
        // dropping the writer channels below (via Generation drop) closes
        // the TCP write halves and engines exit through the EOF path.
        // Nothing to do here that can't happen via field drops.
    }
}

// ---------------------------------------------------------------------------
// Reader / writer task helpers
// ---------------------------------------------------------------------------

fn spawn_writer(
    mut sink: WireSink<TcpStream>,
    mut rx: mpsc::UnboundedReceiver<WireMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sink.send(&msg).await {
                tracing::warn!("supervisor writer: send error: {e}");
                break;
            }
        }
    })
}

fn spawn_reader(
    mut stream: WireStream<TcpStream>,
    in_flight: InFlight,
    latest_snapshot: Arc<Mutex<Option<GraphSnapshot>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match stream.recv().await {
                Ok(msg) => match msg {
                    WireMessage::Response { id, body } => {
                        let sender = in_flight.lock().await.remove(&id);
                        if let Some(tx) = sender {
                            let _ = tx.send(body);
                        } else {
                            tracing::warn!(
                                "supervisor reader: response for unknown correlation {}",
                                id.0
                            );
                        }
                    }
                    WireMessage::Event(Event::Graph { snapshot }) => {
                        *latest_snapshot.lock().await = Some(snapshot);
                    }
                    WireMessage::Event(Event::Log {
                        subscription_id,
                        entry,
                    }) => {
                        // TODO(phase 6): forward log events to MCP clients
                        // through whatever subscription dispatch the tool
                        // surface ends up using.
                        let _ = (subscription_id, entry);
                    }
                    WireMessage::Request { .. } => {
                        tracing::warn!(
                            "supervisor reader: unexpected Request from engine; dropping"
                        );
                    }
                },
                Err(TransportError::Closed) => {
                    // Connection went away cleanly.
                    break;
                }
                Err(e) => {
                    tracing::warn!("supervisor reader: transport error: {e}");
                    break;
                }
            }
        }
        // Reader exiting → connection dead. Wake any in-flight callers
        // by dropping their senders.
        let mut map = in_flight.lock().await;
        map.clear();
    })
}

// ---------------------------------------------------------------------------
// Public re-exports for the resolution err type so consumers don't have to
// import from routing directly when chaining error conversions.
// ---------------------------------------------------------------------------

impl From<ResolveError> for RpcError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::BadRequest(a) => RpcError::BadRequest(a.to_string()),
            ResolveError::NotFound(top) => {
                RpcError::NotFound(format!("top-task {top} not live"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// In-process spawner for tests
// ---------------------------------------------------------------------------

/// Test-only spawner that runs `engine_server::serve_on` in-process on
/// a freshly-bound listener. Used by integration and unit tests that
/// can't `Command::new(current_exe())` the engine.
///
/// Supports a fail-next queue: tests can stage one or more synthetic
/// `SpawnError::EngineExited` errors for upcoming spawns, simulating
/// `cargo build` failures without involving actual compilation.
pub struct InProcessSpawner {
    pub registry: Arc<crate::task::Registry>,
    failures: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

/// Shareable handle to the same failure queue an `InProcessSpawner`
/// uses. Held by tests so they can stage failures after the supervisor
/// has consumed ownership of the spawner box.
#[derive(Clone)]
pub struct FailureHandle {
    queue: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
}

impl FailureHandle {
    /// Queue a synthetic spawn failure. The next call to `spawn` will
    /// return `SpawnError::EngineExited(message)` instead of bringing
    /// up an in-process engine.
    pub fn fail_next(&self, message: String) {
        self.queue.lock().expect("poisoned").push_back(message);
    }
}

impl InProcessSpawner {
    pub fn new(registry: Arc<crate::task::Registry>) -> Self {
        Self {
            registry,
            failures: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Queue a synthetic spawn failure on this spawner directly. Useful
    /// when the caller still owns the spawner. After the supervisor
    /// consumes the box, prefer [`InProcessSpawner::failure_handle`].
    pub fn fail_next(&self, message: String) {
        self.failures.lock().expect("poisoned").push_back(message);
    }

    /// Cloneable handle to the failure queue. Allows tests to stage
    /// failures *after* the supervisor has taken ownership of the
    /// spawner.
    pub fn failure_handle(&self) -> FailureHandle {
        FailureHandle {
            queue: Arc::clone(&self.failures),
        }
    }
}

impl EngineSpawner for InProcessSpawner {
    fn spawn<'a>(
        &'a self,
        start_task_id: u64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<SpawnedEngine, SpawnError>> + Send + 'a>>
    {
        // Drain any queued failure synchronously so the test ordering
        // is deterministic regardless of when the future is awaited.
        let staged = self.failures.lock().expect("poisoned").pop_front();
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            if let Some(msg) = staged {
                return Err(SpawnError::EngineExited(msg));
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            let handle = tokio::spawn(async move {
                let _ = crate::mcp::engine_server::serve_on(listener, registry, start_task_id).await;
            });
            Ok(SpawnedEngine {
                port,
                guard: Box::new(AbortOnDrop(Some(handle))),
            })
        })
    }
}

struct AbortOnDrop(Option<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Registry;

    /// Build a registry that has at least the `:list` builtin so we can
    /// exercise spawn paths without depending on RUNME.rs files.
    fn test_registry() -> Arc<Registry> {
        Arc::new(Registry::from_inventory())
    }

    #[tokio::test]
    async fn supervisor_starts_with_one_generation() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");
        assert_eq!(sup.gens.len(), 1, "exactly one initial gen");
        assert!(sup.latest_gen.is_some(), "latest_gen set");
        sup.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_task_registers_engine_map_and_marks_has_had_tasks() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");

        let (addr_str, _initial_seq) = sup
            .spawn_task(":list".into(), vec![], SpawnOptions::default())
            .await
            .expect("spawn :list");

        // Address parses, top is registered, gen flagged.
        let addr = addr_str.parse::<Address>().expect("address parses");
        assert!(sup.engine_map.lookup(addr.top).is_some());

        let gen_id = sup.latest_gen.unwrap();
        let g = sup.gens.get(&gen_id).unwrap();
        assert!(g.has_had_tasks.load(Ordering::Relaxed));

        sup.shutdown().await;
    }

    #[tokio::test]
    async fn graph_after_spawn_includes_top_task() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");

        let (addr_str, _) = sup
            .spawn_task(":list".into(), vec![], SpawnOptions::default())
            .await
            .expect("spawn :list");
        let expected_top = addr_str.parse::<Address>().unwrap().top;

        // Wait briefly for the snapshot event to land.
        let snap = poll_for_top(&sup, expected_top).await;
        assert!(
            snap.top_tasks.iter().any(|t| t.id == addr_str
                || (t.id.parse::<u64>().ok() == Some(expected_top))),
            "expected snapshot to contain top {expected_top}: {snap:?}"
        );

        sup.shutdown().await;
    }

    /// Spin briefly waiting for the supervisor's cached snapshot to
    /// include `top`. The snapshot lands via an async Event::Graph
    /// frame, so it's not guaranteed to be visible synchronously after
    /// `spawn_task` returns.
    async fn poll_for_top(sup: &Supervisor, top: u64) -> RewrittenSnapshot {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snap = sup.graph().await;
            if snap.top_tasks.iter().any(|t| {
                t.id.parse::<Address>()
                    .map(|a| a.top == top)
                    .unwrap_or(false)
            }) {
                return snap;
            }
            if tokio::time::Instant::now() >= deadline {
                return snap;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn request_addr_routes_to_owning_gen() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");

        let (addr_str, initial_seq) = sup
            .spawn_task(":list".into(), vec![], SpawnOptions::default())
            .await
            .expect("spawn :list");
        let addr = addr_str.parse::<Address>().unwrap();

        let resp = sup
            .request_addr(&addr_str, |a| Request::GetLogs {
                task_id: TaskId(a.task),
                since_seq: Some(initial_seq),
                until_seq: None,
                limit: Some(50),
                filter: None,
            })
            .await
            .expect("get_logs ok");
        match resp {
            Response::GetLogs { .. } => {}
            other => panic!("unexpected response: {other:?}"),
        }
        assert_eq!(addr.task, addr.top);

        sup.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");
        sup.shutdown().await;
        sup.shutdown().await; // no panic on double-shutdown
        assert!(sup.gens.is_empty());
        assert!(sup.latest_gen.is_none());
    }

    #[tokio::test]
    async fn rotate_latest_retires_never_had_tasks_gen() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");

        let first_gen = sup.latest_gen.unwrap();
        // No tasks spawned against gen 0 → rotate retires it.
        sup.rotate_latest().await.expect("rotate ok");

        assert_ne!(sup.latest_gen, Some(first_gen));
        assert!(
            !sup.gens.contains_key(&first_gen),
            "never-had-tasks gen retired immediately"
        );

        sup.shutdown().await;
    }

    #[tokio::test]
    async fn rotate_latest_keeps_gen_with_tasks() {
        let spawner = Box::new(InProcessSpawner::new(test_registry()));
        let mut sup = Supervisor::new_with_spawner(spawner)
            .await
            .expect("supervisor up");

        let first_gen = sup.latest_gen.unwrap();
        let _ = sup
            .spawn_task(":list".into(), vec![], SpawnOptions::default())
            .await
            .expect("spawn :list");

        sup.rotate_latest().await.expect("rotate ok");

        assert_ne!(sup.latest_gen, Some(first_gen));
        assert!(
            sup.gens.contains_key(&first_gen),
            "gen with tasks should stay alive"
        );

        sup.shutdown().await;
    }

    // Layout-only assertion — the trait is only meaningful if it's
    // object-safe, since we store `Box<dyn EngineSpawner>`.
    #[test]
    fn engine_spawner_is_object_safe() {
        fn _accepts(_: Box<dyn EngineSpawner>) {}
    }
}
