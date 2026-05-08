//! Build state machine + file watcher driving `Supervisor::rotate_latest`.
//!
//! # What this module owns
//!
//! - [`BuildState`]: tri-state machine (`Idle` / `Rebuilding` /
//!   `LastBuildFailed`) tracking whether a fresh engine generation can be
//!   spawned. Lives behind a `Mutex` on the supervisor.
//! - [`WatchSet`]: a supervisor-owned `notify::RecommendedWatcher` plus
//!   the bookkeeping that turns filesystem events into a single debounced
//!   "please rebuild" signal.
//! - [`spawn_debounce_loop`]: a tokio task consuming raw notify events,
//!   applying meaningful-event + gitignore filters, and forwarding a
//!   single `()` per quiescent debounce window onto a channel the
//!   supervisor's driver loop reads.
//!
//! # What this module does NOT own
//!
//! - **Compilation.** The supervisor never compiles RUNME.rs sources;
//!   spawning a new engine generation re-execs the outer driver with
//!   `--engine`, which goes through the existing `discover + compile`
//!   pipeline transparently. A `cargo build` failure surfaces here as
//!   `SpawnError::EngineExited(stderr)` and is captured into
//!   `BuildState::LastBuildFailed`.
//! - **Tool routing.** Phase 6 wires this module up to the rmcp tool
//!   surface — spawn-shaped tools call [`crate::mcp::supervisor::Supervisor::check_can_spawn`]
//!   before forwarding; existing-state tools bypass it.
//!
//! # Architectural decisions (locked in plan G0)
//!
//! - Generations that have hosted at least one task live forever for the
//!   MCP session.
//! - Generations that never had tasks retire immediately on the next
//!   `rotate_latest`.
//! - Engine spawn failures don't disturb live generations — they only
//!   flip [`BuildState`] to `LastBuildFailed`. Existing-state tools keep
//!   working.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;

use crate::discover::{RUNME_FILENAME, discover};

// ---------------------------------------------------------------------------
// Build state
// ---------------------------------------------------------------------------

/// State of the most recent rebuild attempt.
///
/// - [`BuildState::Idle`] — no rebuild in flight; the latest generation is
///   ready to accept new spawns.
/// - [`BuildState::Rebuilding`] — a debounced edit fired and we're
///   currently spawning a fresh engine. Spawn-shaped tools should park
///   until the transition.
/// - [`BuildState::LastBuildFailed`] — the most recent attempt to spawn
///   a new generation failed (engine exited before printing its port).
///   Existing generations are unaffected; new spawns are refused with the
///   captured stderr until the next successful rebuild.
#[derive(Debug, Clone)]
pub enum BuildState {
    Idle,
    Rebuilding,
    LastBuildFailed { last_failure_output: String },
}

impl BuildState {
    /// Short tag suitable for the MCP `get_build_status` tool.
    pub fn tag(&self) -> &'static str {
        match self {
            BuildState::Idle => "idle",
            BuildState::Rebuilding => "rebuilding",
            BuildState::LastBuildFailed { .. } => "last_build_failed",
        }
    }
}

/// Snapshot of the current build state for read-only inspection.
#[derive(Debug, Clone)]
pub struct BuildStatusInfo {
    pub state: &'static str,
    pub last_failure_output: Option<String>,
}

impl From<&BuildState> for BuildStatusInfo {
    fn from(s: &BuildState) -> Self {
        match s {
            BuildState::LastBuildFailed {
                last_failure_output,
            } => BuildStatusInfo {
                state: s.tag(),
                last_failure_output: Some(last_failure_output.clone()),
            },
            _ => BuildStatusInfo {
                state: s.tag(),
                last_failure_output: None,
            },
        }
    }
}

/// Truncate stderr to the first `max_lines` lines for inclusion in error
/// messages. Agents fetch the full output via `get_build_status`.
pub fn head_of_failure(output: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (i, line) in output.lines().enumerate() {
        if i >= max_lines {
            out.push_str("... (truncated; call get_build_status for full output)\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Configuration knobs
// ---------------------------------------------------------------------------

/// Sliding-window debounce for filesystem events (per design and
/// `t0.r-notify-patterns`). 200ms is short enough to feel responsive,
/// long enough to fold vim/Helix atomic-save flurries into one rebuild.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Number of lines from the captured stderr surfaced inline on a refused
/// spawn attempt. The full output stays available via the
/// `get_build_status` tool.
pub const FAILURE_HEAD_LINES: usize = 12;

/// Channel capacity for the debounced rebuild signal. Bounded so a
/// stuck driver can't grow memory unboundedly; capacity > 1 just smooths
/// over a brief window where the driver is mid-`rotate_latest`.
const REBUILD_CHANNEL_CAP: usize = 8;

// ---------------------------------------------------------------------------
// Snapshot — shared filter inputs for the debounce loop
// ---------------------------------------------------------------------------

/// Snapshot of filter inputs the debounce loop reads on each event.
///
/// Stored behind `Arc<Mutex<>>` so `WatchSet::refresh()` can update it
/// in place after a successful rebuild without restarting the loop.
pub struct FilterSnapshot {
    pub runme_paths: HashSet<PathBuf>,
    pub watched_dirs: HashSet<PathBuf>,
    pub gitignore: Option<Gitignore>,
}

impl FilterSnapshot {
    pub fn empty() -> Self {
        FilterSnapshot {
            runme_paths: HashSet::new(),
            watched_dirs: HashSet::new(),
            gitignore: None,
        }
    }
}

// ---------------------------------------------------------------------------
// WatchSet — supervisor-owned dynamic watcher
// ---------------------------------------------------------------------------

/// Set of directories currently being watched + the live notify watcher.
///
/// The watcher must be kept alive — dropping a `RecommendedWatcher`
/// stops events. The debounce/dispatch loop runs in a separate tokio
/// task; this struct is the handle the supervisor holds onto so it can
/// call [`WatchSet::refresh`] after each successful rebuild.
///
/// Watch policy: every parent directory of a discovered RUNME.rs is
/// watched **non-recursively** (per `t0.r-notify-patterns` finding #2 —
/// children of unrelated subtrees that have their own RUNME.rs are
/// watched separately, so recursive watching would produce duplicate
/// events).
pub struct WatchSet {
    cwd: PathBuf,
    watcher: RecommendedWatcher,
    /// Filter snapshot shared with the debounce loop. Mutated on
    /// `refresh()`; the loop sees changes on its next event.
    filter: Arc<Mutex<FilterSnapshot>>,
    /// Sender exposed for tests that want to bypass the live notify
    /// watcher and synthesize a rebuild signal directly.
    rebuild_tx: mpsc::Sender<()>,
    /// Debounce loop handle — aborted on drop via the JoinHandle's
    /// abort-on-drop semantics (we don't own AbortOnDrop here; if the
    /// supervisor restarts watchers it must abort manually).
    _debounce_handle: JoinHandle<()>,
}

/// Outcome of constructing a [`WatchSet`].
pub struct WatchSetSetup {
    pub watch_set: WatchSet,
    /// Receiver feeding the supervisor's driver loop. The supervisor
    /// awaits this, transitions build state, and calls `rotate_latest`.
    pub rebuild_rx: mpsc::Receiver<()>,
}

impl WatchSet {
    /// Build a fresh watcher for `cwd`. Performs initial RUNME.rs
    /// discovery, brings the live `notify::RecommendedWatcher` online
    /// with one non-recursive subscription per RUNME.rs parent dir, and
    /// spawns the debounce loop.
    ///
    /// Returns the `WatchSet` plus the rebuild-signal receiver for the
    /// supervisor's driver loop.
    pub fn build(cwd: PathBuf) -> notify::Result<WatchSetSetup> {
        // notify → tokio bridge. The closure runs on notify's worker
        // thread; `unbounded_channel::send` is non-blocking and safe to
        // call from any thread.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = event_tx.send(event);
                }
            },
            Config::default(),
        )?;

        let (rebuild_tx, rebuild_rx) = mpsc::channel::<()>(REBUILD_CHANNEL_CAP);
        let filter = Arc::new(Mutex::new(FilterSnapshot::empty()));

        let debounce_handle = spawn_debounce_loop(
            event_rx,
            rebuild_tx.clone(),
            Arc::clone(&filter),
        );

        let mut ws = WatchSet {
            cwd,
            watcher,
            filter,
            rebuild_tx,
            _debounce_handle: debounce_handle,
        };

        // Initial discovery + subscribe. Happens after the loop is
        // spawned so any startup events on freshly-watched dirs are
        // captured.
        ws.refresh_blocking();

        Ok(WatchSetSetup {
            watch_set: ws,
            rebuild_rx,
        })
    }

    /// Re-discover RUNME.rs files and resync the watcher's subscription
    /// set. Called after every successful rebuild — the user may have
    /// added or removed RUNME.rs files between rebuilds.
    pub async fn refresh(&mut self) {
        let (runme_paths, watched_dirs, gitignore) = self.compute_state();
        self.sync_watch_set(&watched_dirs);
        let mut snap = self.filter.lock().await;
        snap.runme_paths = runme_paths;
        snap.watched_dirs = watched_dirs;
        snap.gitignore = gitignore;
    }

    /// Synchronous initial refresh used during construction.
    fn refresh_blocking(&mut self) {
        let (runme_paths, watched_dirs, gitignore) = self.compute_state();
        self.sync_watch_set(&watched_dirs);
        // blocking_lock: the WatchSet was just constructed; no
        // contention is possible because the loop hasn't observed any
        // events yet.
        let mut snap = self.filter.blocking_lock();
        snap.runme_paths = runme_paths;
        snap.watched_dirs = watched_dirs;
        snap.gitignore = gitignore;
    }

    /// Discover RUNME.rs paths and compute the desired watch dir set
    /// and gitignore matcher.
    fn compute_state(&self) -> (HashSet<PathBuf>, HashSet<PathBuf>, Option<Gitignore>) {
        let result = discover(&self.cwd);
        let mut runme_paths = HashSet::new();
        if let Some(n) = &result.nearest {
            runme_paths.insert(n.clone());
        }
        for c in &result.children {
            runme_paths.insert(c.clone());
        }
        let watched_dirs: HashSet<PathBuf> = runme_paths
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();

        // Build a gitignore matcher rooted at the nearest RUNME.rs's
        // directory. Falling back to cwd if there's no RUNME.rs yet
        // means we still drop editor noise during first-time setup.
        let root = result
            .nearest
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.cwd.clone());
        let mut builder = GitignoreBuilder::new(&root);
        let _ = builder.add(root.join(".gitignore"));
        let gitignore = builder.build().ok();

        (runme_paths, watched_dirs, gitignore)
    }

    /// Diff the current `watched_dirs` against the `desired` set on the
    /// live notify watcher; unwatch removed dirs, watch new ones.
    fn sync_watch_set(&mut self, desired: &HashSet<PathBuf>) {
        // The live state is whatever the watcher knows — but notify
        // doesn't expose that, so we mirror it on the filter snapshot.
        // Read the previously-watched set without holding the mutex
        // across the (sync) watcher calls.
        let previous: HashSet<PathBuf> = {
            // Use try_lock — this is called from refresh_blocking on
            // construction (no contention) and from refresh() with the
            // mutex briefly held above. If contended we fall back to
            // empty (so we re-watch everything; safe but redundant).
            self.filter
                .try_lock()
                .map(|s| s.watched_dirs.clone())
                .unwrap_or_default()
        };

        for p in previous.difference(desired) {
            let _ = self.watcher.unwatch(p);
        }
        for p in desired.difference(&previous) {
            if let Err(e) = self.watcher.watch(p, RecursiveMode::NonRecursive) {
                tracing::warn!("watcher: failed to watch {}: {e}", p.display());
            }
        }
    }

    /// Test/integration hook: bypass the live notify watcher and fire a
    /// rebuild signal directly. Used by tests that don't want a real
    /// filesystem dependency.
    pub fn rebuild_tx(&self) -> mpsc::Sender<()> {
        self.rebuild_tx.clone()
    }

    /// Test/integration hook: read the current filter snapshot.
    pub async fn snapshot(&self) -> Vec<PathBuf> {
        let s = self.filter.lock().await;
        s.runme_paths.iter().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

/// Filter for raw notify events. Accept Create/Modify/Remove broadly; we
/// don't narrow Modify variants because macOS atomic-save is sometimes
/// reported as `Modify(Name(Any))` rather than the more specific Data
/// variant — narrowing risks dropping real edits.
pub fn is_meaningful_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Should an event for `path` actually trigger a rebuild?
///
/// Rules (intentionally permissive — the engine spawn pipeline already
/// re-discovers and recompiles, so a false positive only costs one
/// debounced rebuild):
///
/// 1. Skip events with no paths (notify sometimes emits these).
/// 2. Skip files that fail the `.gitignore` matcher (when present).
/// 3. Skip files that aren't `.rs` and aren't an existing RUNME.rs.
/// 4. Skip files outside any watched directory.
/// 5. Skip editor swap/backup files that survive gitignore.
pub fn passes_filter(
    event: &Event,
    runme_paths: &HashSet<PathBuf>,
    watched_dirs: &HashSet<PathBuf>,
    gitignore: Option<&Gitignore>,
) -> bool {
    if event.paths.is_empty() {
        return false;
    }
    event
        .paths
        .iter()
        .any(|p| path_is_relevant(p, runme_paths, watched_dirs, gitignore))
}

fn path_is_relevant(
    path: &Path,
    runme_paths: &HashSet<PathBuf>,
    watched_dirs: &HashSet<PathBuf>,
    gitignore: Option<&Gitignore>,
) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };

    // Editor backup / swap files (most are .gitignored, but be explicit
    // since the .swp variants sometimes live next to source without
    // .gitignore coverage).
    if file_name.ends_with('~')
        || file_name.starts_with(".#")
        || file_name.ends_with(".swp")
        || file_name.ends_with(".swo")
        || file_name == ".DS_Store"
    {
        return false;
    }

    // Existing RUNME.rs paths bypass extension / dir checks (handles
    // remove events for files we already discovered).
    let is_existing_runme = runme_paths.contains(path);

    // Filename guard. Accept .rs files OR a path named RUNME.rs (handles
    // "user just created RUNME.rs" — not yet in runme_paths because
    // we discovered before this event landed).
    let is_rs = file_name.ends_with(".rs");
    let is_runme = file_name == RUNME_FILENAME;
    if !is_existing_runme && !is_rs && !is_runme {
        return false;
    }

    // Must be inside a watched dir (notify shouldn't deliver otherwise,
    // but defensive — recursive backends sometimes leak).
    let in_watched = watched_dirs
        .iter()
        .any(|d| path == d || path.parent() == Some(d) || path.starts_with(d));
    if !in_watched && !is_existing_runme {
        return false;
    }

    // Gitignore: directly ignored paths are dropped. We don't care about
    // parent-dir matches here because the watch set already excluded
    // ignored dirs at discover time.
    if let Some(g) = gitignore {
        let m = g.matched_path_or_any_parents(path, false);
        if m.is_ignore() {
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Debounce loop
// ---------------------------------------------------------------------------

/// Spawn the debounce/dispatch loop. Consumes raw notify events, applies
/// meaningful + filter checks, and emits exactly one `()` per quiescent
/// 200ms window in which something relevant changed.
///
/// The loop owns the event receiver and the sender for rebuild signals.
/// It reads filter inputs through the shared `Arc<Mutex<FilterSnapshot>>`
/// so `WatchSet::refresh()` can update them without restarting the loop.
pub fn spawn_debounce_loop(
    mut event_rx: mpsc::UnboundedReceiver<Event>,
    rebuild_tx: mpsc::Sender<()>,
    filter: Arc<Mutex<FilterSnapshot>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            if let Some(dl) = deadline {
                tokio::select! {
                    biased;
                    maybe = event_rx.recv() => match maybe {
                        Some(ev) if is_meaningful_event(&ev.kind) => {
                            let snap = filter.lock().await;
                            if passes_filter(
                                &ev,
                                &snap.runme_paths,
                                &snap.watched_dirs,
                                snap.gitignore.as_ref(),
                            ) {
                                deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                            }
                        }
                        Some(_) => {}
                        None => return,
                    },
                    _ = tokio::time::sleep_until(dl) => {
                        // Best-effort send. If the receiver dropped, the
                        // supervisor is shutting down — exit cleanly.
                        if rebuild_tx.send(()).await.is_err() {
                            return;
                        }
                        deadline = None;
                    }
                }
            } else {
                match event_rx.recv().await {
                    Some(ev) if is_meaningful_event(&ev.kind) => {
                        let snap = filter.lock().await;
                        if passes_filter(
                            &ev,
                            &snap.runme_paths,
                            &snap.watched_dirs,
                            snap.gitignore.as_ref(),
                        ) {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                    }
                    Some(_) => {}
                    None => return,
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Driver-loop shared state
// ---------------------------------------------------------------------------

/// Shared state the supervisor's driver loop reads/writes. Public so the
/// supervisor can construct it directly; not used outside the supervisor.
pub struct DriverHandles {
    pub build_state: Arc<Mutex<BuildState>>,
    pub build_state_changed: Arc<Notify>,
}

impl DriverHandles {
    pub fn new() -> Self {
        DriverHandles {
            build_state: Arc::new(Mutex::new(BuildState::Idle)),
            build_state_changed: Arc::new(Notify::new()),
        }
    }

    /// Atomically transition build state and notify all waiters.
    pub async fn set(&self, state: BuildState) {
        *self.build_state.lock().await = state;
        self.build_state_changed.notify_waiters();
    }
}

impl Default for DriverHandles {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_state_tags() {
        assert_eq!(BuildState::Idle.tag(), "idle");
        assert_eq!(BuildState::Rebuilding.tag(), "rebuilding");
        assert_eq!(
            BuildState::LastBuildFailed {
                last_failure_output: String::new()
            }
            .tag(),
            "last_build_failed"
        );
    }

    #[test]
    fn head_of_failure_truncates() {
        let s = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let head = head_of_failure(&s, 5);
        let lines: Vec<&str> = head.lines().collect();
        assert_eq!(lines.len(), 6); // 5 lines + truncation marker
        assert!(lines.last().unwrap().contains("truncated"));
    }

    #[test]
    fn head_of_failure_short_input() {
        let head = head_of_failure("only one line", 12);
        assert_eq!(head, "only one line\n");
    }

    #[test]
    fn is_meaningful_drops_other_and_access() {
        use notify::event::AccessKind;
        assert!(!is_meaningful_event(&EventKind::Other));
        assert!(!is_meaningful_event(&EventKind::Access(AccessKind::Any)));
    }

    #[test]
    fn is_meaningful_accepts_modify_create_remove() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert!(is_meaningful_event(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_meaningful_event(&EventKind::Create(CreateKind::Any)));
        assert!(is_meaningful_event(&EventKind::Remove(RemoveKind::Any)));
    }

    #[test]
    fn passes_filter_skips_non_rs() {
        let mut watched = HashSet::new();
        watched.insert(PathBuf::from("/proj"));
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(PathBuf::from("/proj/notes.txt"));
        assert!(!passes_filter(&event, &HashSet::new(), &watched, None));
    }

    #[test]
    fn passes_filter_accepts_rs_in_watched_dir() {
        let mut watched = HashSet::new();
        watched.insert(PathBuf::from("/proj"));
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(PathBuf::from("/proj/lib.rs"));
        assert!(passes_filter(&event, &HashSet::new(), &watched, None));
    }

    #[test]
    fn passes_filter_drops_swp_files() {
        let mut watched = HashSet::new();
        watched.insert(PathBuf::from("/proj"));
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(PathBuf::from("/proj/.RUNME.rs.swp"));
        assert!(!passes_filter(&event, &HashSet::new(), &watched, None));
    }

    #[test]
    fn passes_filter_accepts_existing_runme() {
        let mut watched = HashSet::new();
        watched.insert(PathBuf::from("/proj"));
        let mut runme = HashSet::new();
        runme.insert(PathBuf::from("/proj/RUNME.rs"));
        let event = Event::new(EventKind::Remove(notify::event::RemoveKind::Any))
            .add_path(PathBuf::from("/proj/RUNME.rs"));
        assert!(passes_filter(&event, &runme, &watched, None));
    }

    #[tokio::test]
    async fn debounce_coalesces_burst() {
        // Drive the loop directly with a burst of events; expect
        // exactly one rebuild signal.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (rebuild_tx, mut rebuild_rx) = mpsc::channel::<()>(8);
        let filter = Arc::new(Mutex::new(FilterSnapshot {
            runme_paths: HashSet::new(),
            watched_dirs: {
                let mut s = HashSet::new();
                s.insert(PathBuf::from("/proj"));
                s
            },
            gitignore: None,
        }));
        let _h = spawn_debounce_loop(event_rx, rebuild_tx, filter);

        // Burst: 5 events within 50ms.
        for _ in 0..5 {
            event_tx
                .send(
                    Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                        .add_path(PathBuf::from("/proj/lib.rs")),
                )
                .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // First rebuild fires after debounce window quiesces.
        let r = tokio::time::timeout(Duration::from_millis(500), rebuild_rx.recv()).await;
        assert!(matches!(r, Ok(Some(()))), "expected one rebuild signal");

        // No second signal in the next 200ms — burst was coalesced.
        let r2 = tokio::time::timeout(Duration::from_millis(200), rebuild_rx.recv()).await;
        assert!(r2.is_err(), "expected no second rebuild signal: {r2:?}");
    }

    #[tokio::test]
    async fn debounce_simulated_atomic_save() {
        // vim/Helix style: Remove then Create on the same path.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (rebuild_tx, mut rebuild_rx) = mpsc::channel::<()>(8);
        let filter = Arc::new(Mutex::new(FilterSnapshot {
            runme_paths: {
                let mut s = HashSet::new();
                s.insert(PathBuf::from("/proj/RUNME.rs"));
                s
            },
            watched_dirs: {
                let mut s = HashSet::new();
                s.insert(PathBuf::from("/proj"));
                s
            },
            gitignore: None,
        }));
        let _h = spawn_debounce_loop(event_rx, rebuild_tx, filter);

        event_tx
            .send(
                Event::new(EventKind::Remove(notify::event::RemoveKind::Any))
                    .add_path(PathBuf::from("/proj/RUNME.rs")),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        event_tx
            .send(
                Event::new(EventKind::Create(notify::event::CreateKind::Any))
                    .add_path(PathBuf::from("/proj/RUNME.rs")),
            )
            .unwrap();

        let r = tokio::time::timeout(Duration::from_millis(500), rebuild_rx.recv()).await;
        assert!(matches!(r, Ok(Some(()))), "expected exactly one rebuild");

        let r2 = tokio::time::timeout(Duration::from_millis(200), rebuild_rx.recv()).await;
        assert!(r2.is_err(), "expected no second signal");
    }
}
