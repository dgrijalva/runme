use std::collections::HashMap;

use tokio::sync::broadcast;

use super::LogEntry;
use super::field_stats::FieldStats;

/// A composition layer for log entries from multiple sources.
///
/// LogStore aggregates log entries from multiple sources (tasks/commands) and
/// provides composition, filtering, grouping, and live subscription capabilities.
/// Each source's entries are stored separately; composition merges them on demand.
///
/// This is the "multi-source composition" layer described in the design doc.
/// Individual `OutputBuffer`s remain the per-process storage mechanism; LogStore
/// composes across them.
pub struct LogStore {
    /// Entries grouped by source name.
    sources: HashMap<String, Vec<LogEntry>>,
    /// Maximum total entries across all sources. When exceeded, oldest entries
    /// (by seq within each source) are dropped from the largest source.
    capacity: Option<usize>,
    /// Broadcast channel for live subscription to new entries.
    tx: broadcast::Sender<LogEntry>,
    /// Per-source field importance stats, updated as entries arrive.
    field_stats: FieldStats,
}

impl LogStore {
    /// Create a new LogStore with no capacity limit.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            sources: HashMap::new(),
            capacity: None,
            tx,
            field_stats: FieldStats::new(),
        }
    }

    /// Create a new LogStore with a maximum total entry count.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            sources: HashMap::new(),
            capacity: Some(capacity),
            tx,
            field_stats: FieldStats::new(),
        }
    }

    /// Add a single entry. The entry's `source` field determines which source
    /// bucket it lands in.
    pub fn push(&mut self, entry: LogEntry) {
        // Broadcast to live subscribers (ignore errors -- no receivers is OK)
        let _ = self.tx.send(entry.clone());

        self.field_stats.observe(&entry.source, &entry.fields);
        let source = entry.source.clone();
        self.sources.entry(source).or_default().push(entry);

        // Enforce capacity if set
        if let Some(cap) = self.capacity {
            self.enforce_capacity(cap);
        }
    }

    /// Add multiple entries from a source. Each entry's `source` field is used
    /// for bucketing (the entries don't all have to share the same source).
    pub fn extend(&mut self, entries: impl IntoIterator<Item = LogEntry>) {
        for entry in entries {
            self.push(entry);
        }
    }

    /// Ingest all current entries from an OutputBuffer snapshot.
    pub fn ingest_buffer(&mut self, buffer: &super::buffer::OutputBuffer) {
        for entry in buffer.lines().iter() {
            self.push(entry.clone());
        }
    }

    /// Compose all sources into a single ordered stream, sorted by `seq`.
    ///
    /// Uses a simple linear merge across all sources. Entries with the same `seq`
    /// are ordered by source name for determinism.
    pub fn compose(&self) -> Vec<&LogEntry> {
        let mut all: Vec<&LogEntry> = self.sources.values().flat_map(|v| v.iter()).collect();
        all.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.source.cmp(&b.source)));
        all
    }

    /// Compose all sources into a single ordered stream, returning owned clones.
    pub fn compose_owned(&self) -> Vec<LogEntry> {
        let mut all: Vec<LogEntry> = self
            .sources
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        all.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.source.cmp(&b.source)));
        all
    }

    /// Compose with a filter applied. Returns only entries matching the predicate.
    ///
    /// The filter is accepted as `impl Fn(&LogEntry) -> bool` so it can be
    /// plugged in from any source (including the filter engine being built in
    /// parallel). The underlying data is not mutated.
    pub fn compose_filtered(&self, filter: impl Fn(&LogEntry) -> bool) -> Vec<&LogEntry> {
        let mut all: Vec<&LogEntry> = self
            .sources
            .values()
            .flat_map(|v| v.iter())
            .filter(|e| filter(e))
            .collect();
        all.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.source.cmp(&b.source)));
        all
    }

    /// Get entries for a single source.
    pub fn source_entries(&self, source: &str) -> Option<&[LogEntry]> {
        self.sources.get(source).map(|v| v.as_slice())
    }

    /// List all source names.
    pub fn source_names(&self) -> Vec<&str> {
        self.sources.keys().map(|s| s.as_str()).collect()
    }

    /// Group entries by source name.
    pub fn group_by_source(&self) -> HashMap<&str, Vec<&LogEntry>> {
        let mut groups: HashMap<&str, Vec<&LogEntry>> = HashMap::new();
        for (source, entries) in &self.sources {
            groups.insert(source.as_str(), entries.iter().collect());
        }
        groups
    }

    /// Group entries by level value.
    pub fn group_by_level(&self) -> HashMap<String, Vec<&LogEntry>> {
        self.group_by(|entry| entry.level.clone().unwrap_or_else(|| "(none)".to_string()))
    }

    /// Group entries by an arbitrary key function.
    ///
    /// The key function extracts a grouping key from each entry. Returns a map
    /// from key to the entries that produced that key.
    pub fn group_by(
        &self,
        key_fn: impl Fn(&LogEntry) -> String,
    ) -> HashMap<String, Vec<&LogEntry>> {
        let mut groups: HashMap<String, Vec<&LogEntry>> = HashMap::new();
        for entry in self.sources.values().flat_map(|v| v.iter()) {
            let key = key_fn(entry);
            groups.entry(key).or_default().push(entry);
        }
        groups
    }

    /// Subscribe to new entries as they are pushed into the store.
    ///
    /// Returns a broadcast receiver. New entries pushed via `push()` or `extend()`
    /// will be sent to all subscribers.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }

    /// Get a reference to the broadcast sender.
    ///
    /// Useful for passing to streaming utilities (e.g., `stream::tail()`).
    pub fn sender(&self) -> &broadcast::Sender<LogEntry> {
        &self.tx
    }

    /// Subscribe with a filter. Returns a `FilteredSubscription` that wraps the
    /// broadcast receiver and only yields entries matching the predicate.
    pub fn subscribe_filtered<F>(&self, filter: F) -> FilteredSubscription<F>
    where
        F: Fn(&LogEntry) -> bool,
    {
        FilteredSubscription {
            rx: self.tx.subscribe(),
            filter,
        }
    }

    /// Total number of entries across all sources.
    pub fn len(&self) -> usize {
        self.sources.values().map(|v| v.len()).sum()
    }

    /// Whether the store contains no entries.
    pub fn is_empty(&self) -> bool {
        self.sources.values().all(|v| v.is_empty())
    }

    /// The capacity limit, if set.
    pub fn capacity(&self) -> Option<usize> {
        self.capacity
    }

    /// Per-source field importance statistics.
    pub fn field_stats(&self) -> &FieldStats {
        &self.field_stats
    }

    /// Enforce the capacity limit by dropping the oldest entries from the largest
    /// source until we're within bounds.
    fn enforce_capacity(&mut self, cap: usize) {
        while self.len() > cap {
            // Find the source with the most entries and remove its oldest
            if let Some(largest_source) = self
                .sources
                .iter()
                .max_by_key(|(_, v)| v.len())
                .map(|(k, _)| k.clone())
            {
                if let Some(entries) = self.sources.get_mut(&largest_source) {
                    if !entries.is_empty() {
                        entries.remove(0);
                    }
                    // Clean up empty sources
                    if entries.is_empty() {
                        self.sources.remove(&largest_source);
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Create an `Output` handle backed by a snapshot of all entries, with
    /// live forwarding of new entries.
    ///
    /// The returned `Output` contains all current entries and will receive
    /// new entries as they are pushed into this LogStore.
    pub fn output(&self) -> crate::process::Output {
        let total = self.len();
        let mut buffer = super::buffer::OutputBuffer::new(total.max(1024));
        for entry in self.compose() {
            buffer.push(entry.clone());
        }

        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(buffer));

        // Forward new entries from the LogStore to the buffer
        let mut rx = self.subscribe();
        let buf = buffer.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(entry) => {
                        buf.lock().await.push(entry);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        crate::process::Output(buffer)
    }

    /// Create an `Output` handle filtered to entries from a single source.
    ///
    /// The returned `Output` contains existing entries from the named source
    /// and will receive new entries for that source as they arrive.
    pub fn output_for(&self, source: &str) -> crate::process::Output {
        let source_entries = self.source_entries(source);
        let count = source_entries.map_or(0, |e| e.len());
        let mut buffer = super::buffer::OutputBuffer::new(count.max(1024));
        if let Some(entries) = source_entries {
            for entry in entries {
                buffer.push(entry.clone());
            }
        }

        let buffer = std::sync::Arc::new(tokio::sync::Mutex::new(buffer));

        // Forward new entries matching the source
        let mut rx = self.subscribe();
        let buf = buffer.clone();
        let source_name = source.to_string();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(entry) => {
                        if entry.source == source_name {
                            buf.lock().await.push(entry);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        crate::process::Output(buffer)
    }
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new()
    }
}

/// A filtered subscription that wraps a broadcast receiver.
///
/// Calling `recv()` will skip entries that don't match the filter and return
/// the next matching entry.
pub struct FilteredSubscription<F> {
    rx: broadcast::Receiver<LogEntry>,
    filter: F,
}

impl<F> FilteredSubscription<F>
where
    F: Fn(&LogEntry) -> bool,
{
    /// Receive the next entry that matches the filter.
    ///
    /// Skips non-matching entries. Returns errors from the underlying broadcast
    /// channel (e.g., lagged or closed).
    pub async fn recv(&mut self) -> Result<LogEntry, broadcast::error::RecvError> {
        loop {
            let entry = self.rx.recv().await?;
            if (self.filter)(&entry) {
                return Ok(entry);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::ParsedContent;
    use crate::log::buffer::OutputBuffer;

    /// Helper to create a LogEntry for testing.
    fn make_entry(source: &str, seq: u64, raw: &str) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
            stream: None,
        }
    }

    /// Helper to create a LogEntry with a level.
    fn make_entry_with_level(source: &str, seq: u64, raw: &str, level: &str) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq,
            timestamp: None,
            level: Some(level.to_string()),
            message: None,
            fields: HashMap::new(),
            stream: None,
        }
    }

    // ---------------------------------------------------------------
    // Composition ordering tests
    // ---------------------------------------------------------------

    #[test]
    fn test_compose_single_source() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "line1"));
        store.push(make_entry("task_a", 1, "line2"));
        store.push(make_entry("task_a", 2, "line3"));

        let composed = store.compose();
        assert_eq!(composed.len(), 3);
        assert_eq!(composed[0].raw, "line1");
        assert_eq!(composed[1].raw, "line2");
        assert_eq!(composed[2].raw, "line3");
    }

    #[test]
    fn test_compose_multi_source_interleaved() {
        let mut store = LogStore::new();
        // Two sources with interleaved seq numbers
        store.push(make_entry("task_a", 0, "a0"));
        store.push(make_entry("task_b", 1, "b1"));
        store.push(make_entry("task_a", 2, "a2"));
        store.push(make_entry("task_b", 3, "b3"));

        let composed = store.compose();
        assert_eq!(composed.len(), 4);
        assert_eq!(composed[0].raw, "a0");
        assert_eq!(composed[1].raw, "b1");
        assert_eq!(composed[2].raw, "a2");
        assert_eq!(composed[3].raw, "b3");
    }

    #[test]
    fn test_compose_same_seq_deterministic() {
        let mut store = LogStore::new();
        // Two entries with the same seq from different sources
        store.push(make_entry("task_b", 0, "b0"));
        store.push(make_entry("task_a", 0, "a0"));

        let composed = store.compose();
        assert_eq!(composed.len(), 2);
        // Should be sorted by source name when seq is equal
        assert_eq!(composed[0].raw, "a0"); // "task_a" < "task_b"
        assert_eq!(composed[1].raw, "b0");
    }

    #[test]
    fn test_compose_owned() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "a0"));
        store.push(make_entry("task_b", 1, "b1"));

        let composed = store.compose_owned();
        assert_eq!(composed.len(), 2);
        assert_eq!(composed[0].raw, "a0");
        assert_eq!(composed[1].raw, "b1");
    }

    #[test]
    fn test_compose_empty() {
        let store = LogStore::new();
        assert!(store.compose().is_empty());
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    // ---------------------------------------------------------------
    // Filtered view tests
    // ---------------------------------------------------------------

    #[test]
    fn test_compose_filtered_by_source() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "a0"));
        store.push(make_entry("task_b", 1, "b1"));
        store.push(make_entry("task_a", 2, "a2"));
        store.push(make_entry("task_b", 3, "b3"));

        let filtered = store.compose_filtered(|e| e.source == "task_a");
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].raw, "a0");
        assert_eq!(filtered[1].raw, "a2");
    }

    #[test]
    fn test_compose_filtered_by_level() {
        let mut store = LogStore::new();
        store.push(make_entry_with_level("app", 0, "info msg", "info"));
        store.push(make_entry_with_level("app", 1, "error msg", "error"));
        store.push(make_entry_with_level("app", 2, "debug msg", "debug"));
        store.push(make_entry_with_level("app", 3, "error msg 2", "error"));

        let errors = store.compose_filtered(|e| e.level.as_deref() == Some("error"));
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].raw, "error msg");
        assert_eq!(errors[1].raw, "error msg 2");
    }

    #[test]
    fn test_compose_filtered_no_matches() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "hello"));

        let filtered = store.compose_filtered(|_| false);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_compose_filtered_all_match() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "a"));
        store.push(make_entry("task_b", 1, "b"));

        let filtered = store.compose_filtered(|_| true);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_compose_filtered_preserves_order() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "a0"));
        store.push(make_entry("task_b", 1, "b1"));
        store.push(make_entry("task_a", 2, "a2"));
        store.push(make_entry("task_b", 3, "b3"));
        store.push(make_entry("task_a", 4, "a4"));

        // Filter to task_b only, verify order is preserved
        let filtered = store.compose_filtered(|e| e.source == "task_b");
        assert_eq!(filtered.len(), 2);
        assert!(filtered[0].seq < filtered[1].seq);
    }

    // ---------------------------------------------------------------
    // Grouping tests
    // ---------------------------------------------------------------

    #[test]
    fn test_group_by_source() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "a0"));
        store.push(make_entry("task_b", 1, "b1"));
        store.push(make_entry("task_a", 2, "a2"));

        let groups = store.group_by_source();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups["task_a"].len(), 2);
        assert_eq!(groups["task_b"].len(), 1);
    }

    #[test]
    fn test_group_by_level() {
        let mut store = LogStore::new();
        store.push(make_entry_with_level("app", 0, "info1", "info"));
        store.push(make_entry_with_level("app", 1, "error1", "error"));
        store.push(make_entry_with_level("app", 2, "info2", "info"));
        store.push(make_entry("app", 3, "no_level")); // No level -> "(none)"

        let groups = store.group_by_level();
        assert_eq!(groups["info"].len(), 2);
        assert_eq!(groups["error"].len(), 1);
        assert_eq!(groups["(none)"].len(), 1);
    }

    #[test]
    fn test_group_by_arbitrary() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "short"));
        store.push(make_entry("task_b", 1, "this is a longer line"));
        store.push(make_entry("task_a", 2, "tiny"));
        store.push(make_entry("task_b", 3, "also quite long enough"));

        // Group by line length bucket
        let groups = store.group_by(|entry| {
            if entry.raw.len() > 10 {
                "long".to_string()
            } else {
                "short".to_string()
            }
        });
        assert_eq!(groups["short"].len(), 2);
        assert_eq!(groups["long"].len(), 2);
    }

    // ---------------------------------------------------------------
    // Live subscription tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn test_subscribe_receives_new_entries() {
        let mut store = LogStore::new();
        let mut rx = store.subscribe();

        store.push(make_entry("task_a", 0, "hello"));

        let received = rx.recv().await.unwrap();
        assert_eq!(received.raw, "hello");
        assert_eq!(received.source, "task_a");
    }

    #[tokio::test]
    async fn test_subscribe_multiple_entries() {
        let mut store = LogStore::new();
        let mut rx = store.subscribe();

        store.push(make_entry("a", 0, "first"));
        store.push(make_entry("b", 1, "second"));
        store.push(make_entry("a", 2, "third"));

        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();
        let e3 = rx.recv().await.unwrap();
        assert_eq!(e1.raw, "first");
        assert_eq!(e2.raw, "second");
        assert_eq!(e3.raw, "third");
    }

    #[tokio::test]
    async fn test_subscribe_filtered() {
        let mut store = LogStore::new();
        let mut filtered_rx = store.subscribe_filtered(|e| e.source == "important");

        store.push(make_entry("noise", 0, "ignore me"));
        store.push(make_entry("important", 1, "pay attention"));
        store.push(make_entry("noise", 2, "also ignore"));

        let received = filtered_rx.recv().await.unwrap();
        assert_eq!(received.raw, "pay attention");
        assert_eq!(received.source, "important");
    }

    #[tokio::test]
    async fn test_subscribe_filtered_by_level() {
        let mut store = LogStore::new();
        let mut errors_rx = store.subscribe_filtered(|e| e.level.as_deref() == Some("error"));

        store.push(make_entry_with_level("app", 0, "info msg", "info"));
        store.push(make_entry_with_level("app", 1, "error msg", "error"));
        store.push(make_entry_with_level("app", 2, "debug msg", "debug"));

        let received = errors_rx.recv().await.unwrap();
        assert_eq!(received.raw, "error msg");
    }

    // ---------------------------------------------------------------
    // Capacity limit tests
    // ---------------------------------------------------------------

    #[test]
    fn test_capacity_bounded() {
        let mut store = LogStore::with_capacity(3);
        store.push(make_entry("a", 0, "line0"));
        store.push(make_entry("a", 1, "line1"));
        store.push(make_entry("a", 2, "line2"));
        assert_eq!(store.len(), 3);

        // Push one more, should drop oldest
        store.push(make_entry("a", 3, "line3"));
        assert_eq!(store.len(), 3);

        let composed = store.compose();
        assert_eq!(composed[0].raw, "line1");
        assert_eq!(composed[1].raw, "line2");
        assert_eq!(composed[2].raw, "line3");
    }

    #[test]
    fn test_capacity_multi_source() {
        let mut store = LogStore::with_capacity(4);
        store.push(make_entry("a", 0, "a0"));
        store.push(make_entry("a", 1, "a1"));
        store.push(make_entry("b", 2, "b0"));
        store.push(make_entry("b", 3, "b1"));
        assert_eq!(store.len(), 4);

        // Push one more; should drop oldest from the largest source
        store.push(make_entry("a", 4, "a2"));
        assert_eq!(store.len(), 4);

        // Source "a" had 2 entries (now 3 after push), source "b" had 2.
        // Largest source is "a" (3 entries after push, before enforce), so a0 gets dropped.
        let a_entries = store.source_entries("a").unwrap();
        assert!(!a_entries.iter().any(|e| e.raw == "a0"));
    }

    #[test]
    fn test_capacity_zero_entries() {
        let store = LogStore::with_capacity(100);
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
    }

    // ---------------------------------------------------------------
    // Source management tests
    // ---------------------------------------------------------------

    #[test]
    fn test_source_entries() {
        let mut store = LogStore::new();
        store.push(make_entry("task_a", 0, "a0"));
        store.push(make_entry("task_b", 1, "b0"));
        store.push(make_entry("task_a", 2, "a1"));

        let a_entries = store.source_entries("task_a").unwrap();
        assert_eq!(a_entries.len(), 2);
        assert_eq!(a_entries[0].raw, "a0");
        assert_eq!(a_entries[1].raw, "a1");

        assert!(store.source_entries("nonexistent").is_none());
    }

    #[test]
    fn test_source_names() {
        let mut store = LogStore::new();
        store.push(make_entry("alpha", 0, "a"));
        store.push(make_entry("beta", 1, "b"));
        store.push(make_entry("alpha", 2, "c"));

        let mut names = store.source_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    // ---------------------------------------------------------------
    // Extend / ingest tests
    // ---------------------------------------------------------------

    #[test]
    fn test_extend() {
        let mut store = LogStore::new();
        let entries = vec![
            make_entry("src", 0, "line0"),
            make_entry("src", 1, "line1"),
            make_entry("other", 2, "other0"),
        ];
        store.extend(entries);

        assert_eq!(store.len(), 3);
        assert_eq!(store.source_entries("src").unwrap().len(), 2);
        assert_eq!(store.source_entries("other").unwrap().len(), 1);
    }

    #[test]
    fn test_ingest_buffer() {
        let mut buffer = OutputBuffer::new(100);
        buffer.push(make_entry("task", 0, "from_buffer_0"));
        buffer.push(make_entry("task", 1, "from_buffer_1"));

        let mut store = LogStore::new();
        store.ingest_buffer(&buffer);

        assert_eq!(store.len(), 2);
        let composed = store.compose();
        assert_eq!(composed[0].raw, "from_buffer_0");
        assert_eq!(composed[1].raw, "from_buffer_1");
    }

    // ---------------------------------------------------------------
    // Default trait
    // ---------------------------------------------------------------

    #[test]
    fn test_default() {
        let store = LogStore::default();
        assert!(store.is_empty());
        assert_eq!(store.capacity(), None);
    }
}
