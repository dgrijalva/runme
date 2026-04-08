use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use globset::Glob;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// The kind of watch, for TUI display purposes.
#[derive(Debug, Clone)]
pub enum WatchKind {
    /// A file glob pattern watch.
    FileGlob(String),
    /// A custom filter function watch.
    Custom,
    /// A raw channel-based watch.
    Channel,
}

/// Metadata about a watch for TUI visibility.
///
/// Shared via `Arc<Mutex<>>` so the TUI can read status without
/// blocking the watch event loop.
#[derive(Debug)]
pub struct WatchInfo {
    /// Optional human-readable label for TUI display.
    pub label: Option<String>,
    /// What kind of watch this is.
    pub kind: WatchKind,
    /// Number of times this watch has triggered (delivered an event to the consumer).
    pub trigger_count: u64,
    /// When the watch last triggered.
    pub last_triggered: Option<Instant>,
}

impl WatchInfo {
    fn new(kind: WatchKind) -> Self {
        Self {
            label: None,
            kind,
            trigger_count: 0,
            last_triggered: None,
        }
    }

    fn record_trigger(&mut self) {
        self.trigger_count += 1;
        self.last_triggered = Some(Instant::now());
    }
}

/// A watch handle that receives events of type `T`.
///
/// Created via `TaskContext::watch()`, `TaskContext::watch_with()`, or
/// `TaskContext::watch_channel()`. Call `.next().await` to wait for the
/// next event.
pub struct Watch<T> {
    rx: mpsc::UnboundedReceiver<T>,
    info: Arc<Mutex<WatchInfo>>,
}

impl<T> Watch<T> {
    /// Wait for the next watch event.
    ///
    /// Blocks until an event is available. Panics if the watch channel
    /// is closed (which only happens if the sender is dropped).
    pub async fn next(&mut self) -> T {
        // Log that we're waiting — shows up when the task is actually blocked on the watch
        if let Ok(info) = self.info.lock() {
            let label = info.label.as_deref().unwrap_or("files");
            let detail = match &info.kind {
                WatchKind::FileGlob(pattern) => format!("pattern={}", pattern),
                WatchKind::Custom => "custom filter".to_string(),
                WatchKind::Channel => "channel".to_string(),
            };
            tracing::info!(watch = label, detail = %detail, "Watching for changes");
        }
        let item = self.rx.recv().await.expect("watch channel closed");
        if let Ok(mut info) = self.info.lock() {
            info.record_trigger();
        }
        item
    }

    /// Set a human-readable label for TUI display.
    ///
    /// Returns `self` for chaining: `ctx.watch("**/*.rs").label("rust sources")`
    pub fn label(self, label: &str) -> Self {
        if let Ok(mut info) = self.info.lock() {
            info.label = Some(label.to_string());
        }
        self
    }

    /// Access the watch info for TUI visibility.
    pub fn info(&self) -> &Arc<Mutex<WatchInfo>> {
        &self.info
    }

    /// Create a new Watch from its parts (for internal use and testing).
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<T>, info: Arc<Mutex<WatchInfo>>) -> Self {
        Self { rx, info }
    }
}

/// Filter paths using a glob pattern.
///
/// Returns the subset of `paths` that match `pattern`. Uses the same glob
/// syntax as `ctx.watch()` — `**` for recursive matching, `*` for single
/// directory level, etc.
///
/// Useful in `ctx.watch_with()` closures that need to categorize changed
/// files into multiple buckets:
///
/// ```ignore
/// let w = ctx.watch_with(|changed| {
///     let rs = glob_filter("src/**/*.rs", changed);
///     let toml = glob_filter("**/Cargo.toml", changed);
///     if rs.is_empty() && toml.is_empty() { None }
///     else { Some((rs, toml)) }
/// });
/// ```
pub fn glob_filter(pattern: &str, paths: &[PathBuf]) -> Vec<PathBuf> {
    let glob = Glob::new(pattern).expect("invalid glob pattern");
    let matcher = glob.compile_matcher();
    paths
        .iter()
        .filter(|p| matcher.is_match(p))
        .cloned()
        .collect()
}

/// Default debounce duration for file watches.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(100);

/// Split a glob pattern into a concrete directory prefix and a glob suffix.
///
/// Everything before the first path component containing a glob character
/// (`*`, `?`, `[`) is treated as a directory path. The remainder is the glob.
///
/// Examples:
///   `crates/**/*.rs`        → ("crates", "**/*.rs")
///   `../../crates/*/src`    → ("../../crates", "*/src")
///   `**/*.rs`               → ("", "**/*.rs")
///   `src/main.rs`           → ("src", "main.rs")  (no glob chars until filename)
fn split_glob_prefix(pattern: &str) -> (&str, &str) {
    let mut last_sep = 0;
    for (i, c) in pattern.char_indices() {
        if c == '*' || c == '?' || c == '[' {
            // Split at the last separator before this glob char
            if last_sep == 0 {
                return ("", pattern);
            }
            return (&pattern[..last_sep], &pattern[last_sep + 1..]);
        }
        if c == '/' {
            last_sep = i;
        }
    }
    // No glob characters found — treat the whole thing as a directory + empty glob
    // This is an edge case; in practice patterns always have glob chars
    if let Some(pos) = pattern.rfind('/') {
        (&pattern[..pos], &pattern[pos + 1..])
    } else {
        ("", pattern)
    }
}

/// Resolve a glob pattern against a base directory.
///
/// Returns the actual directory to watch and the glob pattern to match against.
/// The directory prefix from the pattern is resolved relative to `base_dir`.
fn resolve_watch_target(pattern: &str, base_dir: &PathBuf) -> (PathBuf, String) {
    let (prefix, glob_part) = split_glob_prefix(pattern);
    let actual_dir = if prefix.is_empty() {
        base_dir.clone()
    } else {
        let resolved = base_dir.join(prefix);
        // Canonicalize to resolve .. components, fall back to the joined path
        resolved.canonicalize().unwrap_or(resolved)
    };
    (actual_dir, glob_part.to_string())
}

/// Create a notify watcher that bridges events to a tokio mpsc channel.
///
/// Returns the watcher (must be kept alive) and a receiver for events.
fn create_notify_watcher(
    watch_dir: &PathBuf,
) -> Result<(RecommendedWatcher, mpsc::UnboundedReceiver<Event>), WatchError> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = event_tx.send(event);
            }
        },
        Config::default(),
    )
    .map_err(|e| WatchError::Setup(e.to_string()))?;

    // We need a mutable reference to call watch(), so shadow
    let mut watcher = watcher;
    watcher
        .watch(watch_dir, RecursiveMode::Recursive)
        .map_err(|e| WatchError::Setup(e.to_string()))?;

    Ok((watcher, event_rx))
}

/// Start a file watcher that sends glob-filtered path batches through a channel.
///
/// This is the internal implementation for `ctx.watch()`. It:
/// 1. Creates a `notify::RecommendedWatcher` watching `watch_dir` recursively
/// 2. Collects file events, debounces them over `DEBOUNCE_DURATION`
/// 3. Filters changed paths through the glob `pattern`
/// 4. Sends non-empty batches through the returned channel
pub(crate) fn start_file_watcher(
    pattern: &str,
    watch_dir: PathBuf,
) -> Result<(mpsc::UnboundedReceiver<Vec<PathBuf>>, Arc<Mutex<WatchInfo>>, PathBuf), WatchError> {
    let (actual_dir, glob_part) = resolve_watch_target(pattern, &watch_dir);
    let glob = Glob::new(&glob_part).map_err(|e| WatchError::InvalidGlob(e.to_string()))?;
    let matcher = glob.compile_matcher();
    let info = Arc::new(Mutex::new(WatchInfo::new(WatchKind::FileGlob(
        pattern.to_string(),
    ))));

    let (tx, rx) = mpsc::unbounded_channel();
    let (watcher, event_rx) = create_notify_watcher(&actual_dir)?;

    // Spawn a background task to debounce and filter events
    let dir_clone = actual_dir.clone();
    tokio::spawn(debounce_glob_loop(event_rx, tx, matcher, dir_clone, watcher));

    Ok((rx, info, actual_dir))
}

/// Start a file watcher with a custom filter function.
///
/// Similar to `start_file_watcher` but passes all changed paths through `filter_fn`
/// instead of a glob. The filter returns `Option<T>` — `None` means "not interesting,
/// keep collecting."
pub(crate) fn start_filtered_watcher<F, T>(
    pattern: &str,
    watch_dir: PathBuf,
    filter_fn: F,
) -> Result<(mpsc::UnboundedReceiver<T>, Arc<Mutex<WatchInfo>>, PathBuf), WatchError>
where
    F: Fn(&[PathBuf]) -> Option<T> + Send + 'static,
    T: Send + 'static,
{
    let (actual_dir, _glob_part) = resolve_watch_target(pattern, &watch_dir);
    let info = Arc::new(Mutex::new(WatchInfo::new(WatchKind::Custom)));

    let (tx, rx) = mpsc::unbounded_channel();
    let (watcher, event_rx) = create_notify_watcher(&actual_dir)?;

    // Spawn a background task to debounce and run the filter
    let dir_clone = actual_dir.clone();
    tokio::spawn(debounce_filter_loop(event_rx, tx, filter_fn, dir_clone, watcher));

    Ok((rx, info, actual_dir))
}

/// Background loop: collect notify events, debounce, filter through glob, send batches.
///
/// Keeps `_watcher` alive for the lifetime of the loop (dropping it would stop watching).
async fn debounce_glob_loop(
    mut event_rx: mpsc::UnboundedReceiver<Event>,
    tx: mpsc::UnboundedSender<Vec<PathBuf>>,
    matcher: globset::GlobMatcher,
    watch_dir: PathBuf,
    _watcher: RecommendedWatcher,
) {
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        // If we have a deadline, race between receiving new events and the deadline.
        // If no deadline, just wait for events.
        if let Some(dl) = deadline {
            tokio::select! {
                biased;
                // Check for new events first
                maybe_event = event_rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            if is_meaningful_event(&event.kind) {
                                pending.extend(event.paths);
                                deadline = Some(tokio::time::Instant::now() + DEBOUNCE_DURATION);
                            }
                        }
                        None => return, // watcher dropped
                    }
                }
                // Deadline expired — flush
                _ = tokio::time::sleep_until(dl) => {
                    if !pending.is_empty() {
                        pending.sort();
                        pending.dedup();

                        // Strip watch_dir prefix so relative globs match
                        let matched: Vec<PathBuf> = pending
                            .drain(..)
                            .filter(|p| {
                                let rel = p.strip_prefix(&watch_dir).unwrap_or(p);
                                matcher.is_match(rel)
                            })
                            .collect();

                        if !matched.is_empty() {
                            if tx.send(matched).is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                    deadline = None;
                }
            }
        } else {
            // No pending events — just wait for the next one
            match event_rx.recv().await {
                Some(event) => {
                    if is_meaningful_event(&event.kind) {
                        pending.extend(event.paths);
                        deadline = Some(tokio::time::Instant::now() + DEBOUNCE_DURATION);
                    }
                }
                None => return,
            }
        }
    }
}

/// Background loop: collect notify events, debounce, run through filter function, send results.
async fn debounce_filter_loop<F, T>(
    mut event_rx: mpsc::UnboundedReceiver<Event>,
    tx: mpsc::UnboundedSender<T>,
    filter_fn: F,
    watch_dir: PathBuf,
    _watcher: RecommendedWatcher,
) where
    F: Fn(&[PathBuf]) -> Option<T> + Send + 'static,
    T: Send + 'static,
{
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;

    loop {
        if let Some(dl) = deadline {
            tokio::select! {
                biased;
                maybe_event = event_rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            if is_meaningful_event(&event.kind) {
                                pending.extend(event.paths);
                                deadline = Some(tokio::time::Instant::now() + DEBOUNCE_DURATION);
                            }
                        }
                        None => return,
                    }
                }
                _ = tokio::time::sleep_until(dl) => {
                    if !pending.is_empty() {
                        pending.sort();
                        pending.dedup();

                        // Strip watch_dir prefix so filter sees relative paths
                        let paths: Vec<PathBuf> = pending
                            .drain(..)
                            .map(|p| p.strip_prefix(&watch_dir).map(|r| r.to_path_buf()).unwrap_or(p))
                            .collect();
                        if let Some(value) = filter_fn(&paths) {
                            if tx.send(value).is_err() {
                                return;
                            }
                        }
                    }
                    deadline = None;
                }
            }
        } else {
            match event_rx.recv().await {
                Some(event) => {
                    if is_meaningful_event(&event.kind) {
                        pending.extend(event.paths);
                        deadline = Some(tokio::time::Instant::now() + DEBOUNCE_DURATION);
                    }
                }
                None => return,
            }
        }
    }
}

/// Filter out noise from the event stream.
///
/// Only pass through Create, Modify, and Remove events. Access events,
/// metadata-only events, and other noise are ignored.
fn is_meaningful_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Errors that can occur when setting up a watch.
#[derive(Debug)]
pub enum WatchError {
    /// The glob pattern was invalid.
    InvalidGlob(String),
    /// Failed to set up the filesystem watcher.
    Setup(String),
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchError::InvalidGlob(msg) => write!(f, "invalid glob pattern: {}", msg),
            WatchError::Setup(msg) => write!(f, "watch setup failed: {}", msg),
        }
    }
}

impl std::error::Error for WatchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_glob_filter_matches() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("Cargo.toml"),
            PathBuf::from("src/utils/helper.rs"),
            PathBuf::from("tests/integration.rs"),
            PathBuf::from("README.md"),
        ];

        let result = glob_filter("src/**/*.rs", &paths);
        assert_eq!(result.len(), 3);
        assert!(result.contains(&PathBuf::from("src/main.rs")));
        assert!(result.contains(&PathBuf::from("src/lib.rs")));
        assert!(result.contains(&PathBuf::from("src/utils/helper.rs")));
    }

    #[test]
    fn test_glob_filter_no_matches() {
        let paths = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("Cargo.toml"),
        ];

        let result = glob_filter("**/*.py", &paths);
        assert!(result.is_empty());
    }

    #[test]
    fn test_glob_filter_star_pattern() {
        let paths = vec![
            PathBuf::from("Cargo.toml"),
            PathBuf::from("Cargo.lock"),
            PathBuf::from("src/Cargo.toml"),
        ];

        let result = glob_filter("**/Cargo.toml", &paths);
        assert_eq!(result.len(), 2);
        assert!(result.contains(&PathBuf::from("Cargo.toml")));
        assert!(result.contains(&PathBuf::from("src/Cargo.toml")));
    }

    #[test]
    fn test_glob_filter_empty_paths() {
        let paths: Vec<PathBuf> = vec![];
        let result = glob_filter("**/*.rs", &paths);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_watch_channel_send_receive() {
        let info = Arc::new(Mutex::new(WatchInfo::new(WatchKind::Channel)));
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watch: Watch<String> = Watch::new(rx, info.clone());

        tx.send("hello".to_string()).unwrap();
        tx.send("world".to_string()).unwrap();

        let first = watch.next().await;
        assert_eq!(first, "hello");

        let second = watch.next().await;
        assert_eq!(second, "world");

        // Check trigger count
        let info = watch.info().lock().unwrap();
        assert_eq!(info.trigger_count, 2);
        assert!(info.last_triggered.is_some());
    }

    #[test]
    fn test_watch_label() {
        let info = Arc::new(Mutex::new(WatchInfo::new(WatchKind::Channel)));
        let (_tx, rx) = mpsc::unbounded_channel::<()>();
        let watch: Watch<()> = Watch::new(rx, info);

        let watch = watch.label("my watch");
        let info = watch.info().lock().unwrap();
        assert_eq!(info.label, Some("my watch".to_string()));
    }

    #[test]
    fn test_watch_info_defaults() {
        let info = WatchInfo::new(WatchKind::FileGlob("*.rs".to_string()));
        assert!(info.label.is_none());
        assert_eq!(info.trigger_count, 0);
        assert!(info.last_triggered.is_none());
        assert!(matches!(info.kind, WatchKind::FileGlob(_)));
    }

    #[test]
    fn test_watch_info_record_trigger() {
        let mut info = WatchInfo::new(WatchKind::Custom);
        assert_eq!(info.trigger_count, 0);

        info.record_trigger();
        assert_eq!(info.trigger_count, 1);
        assert!(info.last_triggered.is_some());

        let first_trigger = info.last_triggered.unwrap();
        std::thread::sleep(Duration::from_millis(1));

        info.record_trigger();
        assert_eq!(info.trigger_count, 2);
        assert!(info.last_triggered.unwrap() >= first_trigger);
    }

    #[tokio::test]
    async fn test_file_watch_detects_changes() {
        use std::fs;

        // Create a temp directory with a file
        let tmp = std::env::temp_dir().join(format!("runme_watch_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let (mut rx, _info, _dir) = start_file_watcher("**/*.txt", tmp.clone()).unwrap();

        // Give the watcher time to set up
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Write a file that matches the glob
        fs::write(tmp.join("test.txt"), "hello").unwrap();

        // Wait for the event (with timeout)
        let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(result.is_ok(), "timed out waiting for file watch event");
        let paths = result.unwrap().unwrap();
        assert!(!paths.is_empty());
        assert!(paths.iter().any(|p| p.ends_with("test.txt")));

        // Clean up
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_file_watch_ignores_non_matching() {
        use std::fs;

        let tmp = std::env::temp_dir().join(format!(
            "runme_watch_nomatch_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let (mut rx, _info, _dir) = start_file_watcher("**/*.rs", tmp.clone()).unwrap();

        // Give the watcher time to set up
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Write a file that does NOT match the glob
        fs::write(tmp.join("test.txt"), "hello").unwrap();

        // Should not receive anything within a reasonable time
        let result = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "should not have received an event for non-matching file"
        );

        // Clean up
        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_filtered_watcher() {
        use std::fs;

        let tmp = std::env::temp_dir().join(format!(
            "runme_watch_filter_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Custom filter: only return .rs files, mapped to their count
        let (mut rx, _info, _dir) = start_filtered_watcher("**/*", tmp.clone(), |paths| {
            let rs_files: Vec<PathBuf> = paths
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "rs"))
                .cloned()
                .collect();
            if rs_files.is_empty() {
                None
            } else {
                Some(rs_files.len())
            }
        })
        .unwrap();

        // Give the watcher time to set up
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Write a .rs file
        fs::write(tmp.join("main.rs"), "fn main() {}").unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            result.is_ok(),
            "timed out waiting for filtered watch event"
        );
        let count = result.unwrap().unwrap();
        assert!(count >= 1);

        // Clean up
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_meaningful_event() {
        use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

        assert!(is_meaningful_event(&EventKind::Create(CreateKind::File)));
        assert!(is_meaningful_event(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any
        ))));
        assert!(is_meaningful_event(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_meaningful_event(&EventKind::Access(AccessKind::Read)));
        assert!(!is_meaningful_event(&EventKind::Other));
    }
}
