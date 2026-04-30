//! Process-lifetime monotonic identifier for tasks (and, in later slices,
//! the processes they spawn).
//!
//! `TaskId::ROOT == TaskId(0)` is reserved for the synthetic root task. The
//! allocator (`TaskId::next`) starts at 1 so the root id is never accidentally
//! re-issued. Allocation is process-global (a module-static `AtomicU64`) so
//! ids can be minted from any code path that doesn't have an `Engine`
//! reference (e.g. tests).
//!
//! See `docs/plans/notes/architecture.md` §1.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Process-lifetime monotonic identifier for a task or spawned process.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub u64);

impl TaskId {
    /// The synthetic root task always has id 0. Allocated children start at 1.
    pub const ROOT: TaskId = TaskId(0);

    /// Allocate the next unique `TaskId`. Process-lifetime monotonic.
    pub fn next() -> Self {
        TaskId(NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed))
    }
}

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_zero() {
        assert_eq!(TaskId::ROOT, TaskId(0));
    }

    #[test]
    fn next_starts_above_root_and_is_monotonic() {
        let a = TaskId::next();
        let b = TaskId::next();
        assert!(a.0 >= 1, "TaskId::next must never collide with ROOT");
        assert!(b.0 > a.0, "TaskId::next must be monotonic");
    }

    #[test]
    fn display_format() {
        assert_eq!(TaskId(0).to_string(), "t0");
        assert_eq!(TaskId(42).to_string(), "t42");
    }
}
