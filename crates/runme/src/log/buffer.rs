use std::collections::VecDeque;

use tokio::sync::broadcast;

use super::LogEntry;

/// Output ring buffer for a task.
///
/// Stores log entries with bounded capacity. When full, oldest entries are dropped.
/// Also broadcasts new entries to subscribers.
pub struct OutputBuffer {
    lines: VecDeque<LogEntry>,
    capacity: usize,
    tx: broadcast::Sender<LogEntry>,
}

impl OutputBuffer {
    /// Create a new buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
            tx,
        }
    }

    /// Push a log entry into the buffer. Drops the oldest if at capacity.
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

    /// Get a reference to the broadcast sender.
    ///
    /// Useful for passing to streaming utilities (e.g., `stream::tail()`).
    pub fn subscribe_sender(&self) -> &broadcast::Sender<LogEntry> {
        &self.tx
    }
}
