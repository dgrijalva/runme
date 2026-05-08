use std::collections::VecDeque;

use tokio::sync::broadcast;

use super::{LogEntry, SeqGen};

/// Output ring buffer for a task.
///
/// Stores log entries with bounded capacity. When full, oldest entries are dropped.
/// Also broadcasts new entries to subscribers.
///
/// Owns a clone of the engine-global `SeqGen` so entries are stamped with a
/// monotonic seq at construction time (in `push`). Tests / fixtures that don't
/// have an existing `SeqGen` can construct a fresh one via `SeqGen::new()`.
pub struct OutputBuffer {
    lines: VecDeque<LogEntry>,
    capacity: usize,
    tx: broadcast::Sender<LogEntry>,
    seq_gen: SeqGen,
}

impl OutputBuffer {
    /// Create a new buffer with the given capacity and a fresh `SeqGen`.
    ///
    /// Convenience constructor for tests and other isolated fixtures. Production
    /// code paths should use [`OutputBuffer::with_seq_gen`] so all buffers share
    /// the engine-global counter.
    pub fn new(capacity: usize) -> Self {
        Self::with_seq_gen(capacity, SeqGen::new())
    }

    /// Create a new buffer with the given capacity, sharing the supplied `SeqGen`.
    pub fn with_seq_gen(capacity: usize, seq_gen: SeqGen) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
            tx,
            seq_gen,
        }
    }

    /// Push a log entry into the buffer. Drops the oldest if at capacity.
    ///
    /// Producers are expected to stamp the entry's seq via the buffer's
    /// `seq_gen()` at construction time; this method does not override it.
    /// (The `debug_assert` enforcement lives in `LogStore::push`, the
    /// canonical aggregation point.)
    pub fn push(&mut self, entry: LogEntry) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        // Broadcast to subscribers (ignore errors — no receivers is OK)
        let _ = self.tx.send(entry.clone());
        self.lines.push_back(entry);
    }

    /// Get all buffered entries.
    pub fn lines(&self) -> &VecDeque<LogEntry> {
        &self.lines
    }

    /// Get a broadcast receiver for new entries.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }

    /// Number of entries currently in the buffer.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The maximum capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clone the buffer's `SeqGen`. Useful when a downstream caller needs to
    /// stamp entries without going through `push` (e.g. `TaskContext::println`
    /// constructing a `LogEntry::raw` ahead of forwarding it).
    pub fn seq_gen(&self) -> SeqGen {
        self.seq_gen.clone()
    }

    /// Replace the buffer's `SeqGen`. The engine wires per-task subprocess
    /// output buffers up with `OutputBuffer::new` (default fresh `SeqGen`)
    /// at `TaskContext::new` time, then swaps in the engine-global generator
    /// inside `TaskExecution::spawn_body` once the engine context is known.
    /// Without this swap, subprocess output (`exec`/`spawn`) seqs would be
    /// per-buffer rather than engine-global, breaking `since_seq`-based
    /// subscription and global cross-source ordering.
    pub fn set_seq_gen(&mut self, seq_gen: SeqGen) {
        self.seq_gen = seq_gen;
    }

    /// Get a reference to the broadcast sender.
    ///
    /// Useful for passing to streaming utilities (e.g., `stream::tail()`).
    pub fn subscribe_sender(&self) -> &broadcast::Sender<LogEntry> {
        &self.tx
    }
}
