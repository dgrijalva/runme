//! Routing and addressing layer for the MCP supervisor.
//!
//! This module is pure data transformation: it owns the dotted address
//! grammar agents see on the wire (`<top>(.<task>(.<seq>)?)?`), the
//! supervisor-side map from top-level task ids to their owning generation,
//! and the snapshot rewriter that turns engine-internal `TaskId`s into
//! globally-unique dotted strings.
//!
//! No I/O. No async. No engine handles. The supervisor (a sibling phase)
//! plugs its concrete generation handle type in via the `G` parameter on
//! [`EngineMap`].
//!
//! See `docs/mcp_design.md` § "Identifiers and routing" for the design
//! source of truth.

use std::collections::HashMap;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::execution::TaskId;
use crate::execution::engine::{GraphSnapshot, ProcessNodeInfo, TaskNode};
use crate::execution::execution::{ProcessStatus, TaskStatus};

// ---------------------------------------------------------------------------
// Address
// ---------------------------------------------------------------------------

/// Dotted task/log address parsed from agent-facing strings.
///
/// Format: `<top>(.<task>(.<seq>)?)?`
///
/// - `top` is always the top-level user-spawned task id (routes to a
///   generation).
/// - `task` is the engine-internal `TaskId`; for top-level tasks
///   `task == top`.
/// - `seq` is an optional log sequence cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub top: u64,
    pub task: u64,
    pub seq: Option<u64>,
}

impl std::str::FromStr for Address {
    type Err = AddressError;

    /// Parse a dotted address string. Strict: rejects leading `t`
    /// prefix (display-only), whitespace, empty segments, and non-digit
    /// characters.
    fn from_str(s: &str) -> Result<Self, AddressError> {
        if s.is_empty() {
            return Err(AddressError::Malformed(s.to_string()));
        }
        let mut parts = s.split('.');
        // Always at least one segment (split on non-empty input).
        let top_str = parts.next().unwrap();
        let top = parse_u64_strict(top_str).ok_or_else(|| AddressError::Malformed(s.to_string()))?;

        let task = match parts.next() {
            Some(t) => {
                parse_u64_strict(t).ok_or_else(|| AddressError::Malformed(s.to_string()))?
            }
            None => top,
        };

        let seq = match parts.next() {
            Some(t) => Some(
                parse_u64_strict(t).ok_or_else(|| AddressError::Malformed(s.to_string()))?,
            ),
            None => None,
        };

        // No fourth segment.
        if parts.next().is_some() {
            return Err(AddressError::Malformed(s.to_string()));
        }

        Ok(Address { top, task, seq })
    }
}

impl Address {
    /// Render `<top>` or `<top>.<task>` (omitting `.task` when
    /// `task == top`). Used by the snapshot rewriter for outbound
    /// serialization.
    pub fn render_task(top: u64, task: u64) -> String {
        if top == task {
            top.to_string()
        } else {
            format!("{top}.{task}")
        }
    }
}

/// Strict u64 parser: rejects empty strings, leading/trailing whitespace,
/// signs, and any non-ASCII-digit characters.
fn parse_u64_strict(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("malformed address: {0}")]
    Malformed(String),
}

// ---------------------------------------------------------------------------
// EngineMap
// ---------------------------------------------------------------------------

/// Identifier for a generation, supervisor-assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationId(pub u64);

/// A reference to a generation registered in the [`EngineMap`].
///
/// `G` is application-supplied (likely an `Arc<GenerationState>` or
/// similar). This module never inspects it. Field name is `handle`
/// because `gen` is a reserved keyword in Rust edition 2024.
#[derive(Debug, Clone)]
pub struct GenRef<G> {
    pub gen_id: GenerationId,
    pub handle: G,
    pub retired: bool,
}

/// Maps top-level task ids to the generation that owns them.
///
/// One entry per top-level task ever spawned (across all live + retired
/// gens). Lookups for retired-gen ids return `None`.
pub struct EngineMap<G> {
    by_top: HashMap<u64, GenRef<G>>,
}

impl<G> Default for EngineMap<G> {
    fn default() -> Self {
        Self {
            by_top: HashMap::new(),
        }
    }
}

impl<G: Clone> EngineMap<G> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a top-level task id as belonging to a generation. Called
    /// when a `SpawnTask` response comes back from a generation.
    pub fn insert(&mut self, top: u64, gen_id: GenerationId, handle: G) {
        self.by_top.insert(
            top,
            GenRef {
                gen_id,
                handle,
                retired: false,
            },
        );
    }

    /// Look up the gen that owns this top-level id. Returns `None` if
    /// unknown OR if the gen is retired.
    pub fn lookup(&self, top: u64) -> Option<&GenRef<G>> {
        self.by_top.get(&top).filter(|g| !g.retired)
    }

    /// Mark a generation as retired. Subsequent lookups for any of its
    /// top-task ids return `None`. Used for never-had-tasks gens that
    /// get eclipsed before they spawn anything.
    pub fn retire_generation(&mut self, gen_id: GenerationId) {
        for entry in self.by_top.values_mut() {
            if entry.gen_id == gen_id {
                entry.retired = true;
            }
        }
    }

    /// Iterate all live (non-retired) gens, deduplicated by `gen_id`.
    pub fn live_gens(&self) -> impl Iterator<Item = &GenRef<G>> {
        let mut seen = std::collections::HashSet::new();
        self.by_top.values().filter(move |g| {
            if g.retired {
                return false;
            }
            seen.insert(g.gen_id)
        })
    }
}

// ---------------------------------------------------------------------------
// Snapshot rewriter
// ---------------------------------------------------------------------------

/// Rewritten snapshot: a flat list of top-level task subtrees with
/// engine-internal ids replaced by dotted strings the agent can use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewrittenSnapshot {
    pub top_tasks: Vec<RewrittenTaskNode>,
}

/// Rewritten task node — mirrors [`TaskNode`] but with id fields turned
/// into dotted `<top>(.<task>)?` strings and children inlined as a tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewrittenTaskNode {
    /// `<top>` for top-level, `<top>.<task>` otherwise.
    pub id: String,
    pub name: String,
    /// Dotted form of the parent, or `None` for a top-level task.
    pub parent: Option<String>,
    pub children: Vec<RewrittenTaskNode>,
    pub processes: Vec<RewrittenProcessNode>,
    pub status: TaskStatus,
    pub started_at: Option<DateTime<Local>>,
    pub ended_at: Option<DateTime<Local>>,
    pub summary: Option<String>,
    pub gen_id: GenerationId,
}

/// Rewritten process node — mirrors [`ProcessNodeInfo`] with the id field
/// turned into a dotted string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewrittenProcessNode {
    pub id: String,
    pub task_name: String,
    pub command_label: String,
    pub pid: Option<u32>,
    pub pgid: Option<i32>,
    pub status: ProcessStatus,
    pub ready: bool,
}

impl Serialize for GenerationId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for GenerationId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64::deserialize(d).map(GenerationId)
    }
}

/// Rewrite a single engine-side [`GraphSnapshot`] into a flat list of
/// top-level task subtrees, with each node's id rewritten to
/// `<top>(.<task>)?` form.
pub fn rewrite_snapshot(snapshot: &GraphSnapshot, gen_id: GenerationId) -> RewrittenSnapshot {
    // Find every top-level task id (direct child of ROOT) by walking the
    // `tasks` map. Use this rather than ROOT's `children` list so we
    // don't depend on root tracking; matches `is_top_level`'s rule.
    let mut top_ids: Vec<TaskId> = snapshot
        .tasks
        .iter()
        .filter(|(_, n)| n.parent == Some(TaskId::ROOT))
        .map(|(id, _)| *id)
        .collect();
    top_ids.sort_by_key(|t| t.0);

    let top_tasks = top_ids
        .into_iter()
        .map(|tid| build_subtree(snapshot, tid, tid, gen_id))
        .collect();

    RewrittenSnapshot { top_tasks }
}

fn build_subtree(
    snapshot: &GraphSnapshot,
    top: TaskId,
    id: TaskId,
    gen_id: GenerationId,
) -> RewrittenTaskNode {
    // Defensive: missing nodes shouldn't happen for ids drawn from the
    // map, but if they do, render an empty placeholder rather than panic.
    let Some(node) = snapshot.tasks.get(&id) else {
        return RewrittenTaskNode {
            id: Address::render_task(top.0, id.0),
            name: String::new(),
            parent: None,
            children: Vec::new(),
            processes: Vec::new(),
            status: TaskStatus::Setup,
            started_at: None,
            ended_at: None,
            summary: None,
            gen_id,
        };
    };

    rewrite_node(snapshot, top, node, gen_id)
}

fn rewrite_node(
    snapshot: &GraphSnapshot,
    top: TaskId,
    node: &TaskNode,
    gen_id: GenerationId,
) -> RewrittenTaskNode {
    let id = Address::render_task(top.0, node.id.0);
    let parent = node.parent.and_then(|p| {
        if p == TaskId::ROOT {
            None
        } else {
            Some(Address::render_task(top.0, p.0))
        }
    });

    let mut child_ids = node.children.clone();
    child_ids.sort_by_key(|c| c.0);
    let children = child_ids
        .into_iter()
        .filter_map(|cid| snapshot.tasks.get(&cid))
        .map(|child_node| rewrite_node(snapshot, top, child_node, gen_id))
        .collect();

    let processes = node
        .processes
        .iter()
        .map(|p| rewrite_process(top, p))
        .collect();

    RewrittenTaskNode {
        id,
        name: node.name.clone(),
        parent,
        children,
        processes,
        status: node.status.clone(),
        started_at: node.started_at,
        ended_at: node.ended_at,
        summary: node.summary.clone(),
        gen_id,
    }
}

fn rewrite_process(top: TaskId, p: &ProcessNodeInfo) -> RewrittenProcessNode {
    RewrittenProcessNode {
        id: Address::render_task(top.0, p.id.0),
        task_name: p.task_name.clone(),
        command_label: p.command_label.clone(),
        pid: p.pid,
        pgid: p.pgid,
        status: p.status.clone(),
        ready: p.ready,
    }
}

// ---------------------------------------------------------------------------
// Snapshot merger
// ---------------------------------------------------------------------------

/// Merge rewritten snapshots from every live generation into one flat
/// list, ordered by top-task id ascending. No supervisor-level meta-root.
pub fn merge_snapshots(per_gen: Vec<RewrittenSnapshot>) -> RewrittenSnapshot {
    let mut top_tasks: Vec<RewrittenTaskNode> = per_gen
        .into_iter()
        .flat_map(|s| s.top_tasks.into_iter())
        .collect();
    top_tasks.sort_by_key(|t| {
        // Top-level entries have id == "<top>" (no dot). Parse it; on the
        // off chance something is malformed (it shouldn't be, since we
        // rendered it ourselves), sort it last.
        t.id.parse::<Address>().map(|a| a.top).unwrap_or(u64::MAX)
    });
    RewrittenSnapshot { top_tasks }
}

// ---------------------------------------------------------------------------
// Inbound address resolution
// ---------------------------------------------------------------------------

/// Errors returned by [`resolve_address`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("bad request: {0}")]
    BadRequest(#[from] AddressError),
    /// The given top-level task id is unknown OR its owning generation
    /// has been retired.
    #[error("not found: {0}")]
    NotFound(u64),
}

/// Resolve an inbound dotted address to a generation reference.
pub fn resolve_address<G: Clone>(
    s: &str,
    map: &EngineMap<G>,
) -> Result<(Address, GenRef<G>), ResolveError> {
    let addr = s.parse::<Address>()?;
    let gen_ref = map
        .lookup(addr.top)
        .ok_or(ResolveError::NotFound(addr.top))?
        .clone();
    Ok((addr, gen_ref))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::execution::TaskId;
    use crate::execution::engine::{GraphSnapshot, ProcessNodeInfo, TaskNode};
    use crate::execution::execution::{ProcessStatus, TaskStatus};

    // --- Address::parse ---

    #[test]
    fn address_parse_bare_top() {
        let a = "42".parse::<Address>().unwrap();
        assert_eq!(
            a,
            Address {
                top: 42,
                task: 42,
                seq: None
            }
        );
    }

    #[test]
    fn address_parse_top_task() {
        let a = "42.7".parse::<Address>().unwrap();
        assert_eq!(
            a,
            Address {
                top: 42,
                task: 7,
                seq: None
            }
        );
    }

    #[test]
    fn address_parse_top_top() {
        // top.top is accepted, equivalent to bare top.
        let a = "42.42".parse::<Address>().unwrap();
        assert_eq!(
            a,
            Address {
                top: 42,
                task: 42,
                seq: None
            }
        );
    }

    #[test]
    fn address_parse_top_task_seq() {
        let a = "42.7.100".parse::<Address>().unwrap();
        assert_eq!(
            a,
            Address {
                top: 42,
                task: 7,
                seq: Some(100)
            }
        );
    }

    #[test]
    fn address_parse_top_top_seq() {
        let a = "42.42.100".parse::<Address>().unwrap();
        assert_eq!(
            a,
            Address {
                top: 42,
                task: 42,
                seq: Some(100)
            }
        );
    }

    #[test]
    fn address_parse_zero_is_valid_u64() {
        // Supervisor logic decides if 0 is reserved; parser accepts it.
        let a = "0".parse::<Address>().unwrap();
        assert_eq!(a.top, 0);
    }

    #[test]
    fn address_parse_malformed() {
        for bad in [
            "", "42.", "42.x", "abc", " 42", "42 ", "t42", ".", ".5", "42..7", "42.7.", "42.7.x",
            "42.7.8.9", "-1", "+1", "42.-1",
        ] {
            assert!(
                bad.parse::<Address>().is_err(),
                "expected malformed for {bad:?}"
            );
        }
    }

    #[test]
    fn render_task_omits_top_when_equal() {
        assert_eq!(Address::render_task(42, 42), "42");
        assert_eq!(Address::render_task(42, 7), "42.7");
    }

    // --- EngineMap ---

    #[test]
    fn engine_map_insert_lookup() {
        let mut m: EngineMap<&'static str> = EngineMap::new();
        m.insert(1, GenerationId(10), "g10");
        let r = m.lookup(1).unwrap();
        assert_eq!(r.gen_id, GenerationId(10));
        assert_eq!(r.handle, "g10");
        assert!(!r.retired);
        assert!(m.lookup(2).is_none());
    }

    #[test]
    fn engine_map_retire_hides_lookups() {
        let mut m: EngineMap<&'static str> = EngineMap::new();
        m.insert(1, GenerationId(10), "g10");
        m.insert(2, GenerationId(10), "g10");
        m.insert(3, GenerationId(11), "g11");
        m.retire_generation(GenerationId(10));
        assert!(m.lookup(1).is_none());
        assert!(m.lookup(2).is_none());
        // Other gen still visible.
        assert_eq!(m.lookup(3).unwrap().gen_id, GenerationId(11));
    }

    #[test]
    fn engine_map_live_gens_skips_retired_and_dedupes() {
        let mut m: EngineMap<&'static str> = EngineMap::new();
        m.insert(1, GenerationId(10), "g10");
        m.insert(2, GenerationId(10), "g10"); // same gen, two top tasks
        m.insert(3, GenerationId(11), "g11");
        m.insert(4, GenerationId(12), "g12");
        m.retire_generation(GenerationId(11));

        let mut live: Vec<u64> = m.live_gens().map(|g| g.gen_id.0).collect();
        live.sort();
        assert_eq!(live, vec![10, 12]);
    }

    // --- rewrite_snapshot ---

    fn make_task_node(
        id: TaskId,
        name: &str,
        parent: Option<TaskId>,
        children: Vec<TaskId>,
        processes: Vec<ProcessNodeInfo>,
    ) -> TaskNode {
        TaskNode {
            id,
            name: name.to_string(),
            parent,
            children,
            status: TaskStatus::Setup,
            processes,
            started_at: None,
            ended_at: None,
            summary: None,
        }
    }

    fn make_proc(id: TaskId, label: &str) -> ProcessNodeInfo {
        ProcessNodeInfo {
            id,
            task_name: "task".to_string(),
            command_label: label.to_string(),
            pid: None,
            pgid: None,
            status: ProcessStatus::Running,
            ready: false,
        }
    }

    #[test]
    fn rewrite_snapshot_builds_dotted_subtrees() {
        // Tree:
        //   ROOT(0)
        //    ├── t10 (top)
        //    │     └── t11 (sub)
        //    │           └── proc t99 owned by t11
        //    └── t20 (top)
        let mut tasks = HashMap::new();
        tasks.insert(
            TaskId(10),
            make_task_node(TaskId(10), "alpha", Some(TaskId::ROOT), vec![TaskId(11)], vec![]),
        );
        tasks.insert(
            TaskId(11),
            make_task_node(
                TaskId(11),
                "alpha-sub",
                Some(TaskId(10)),
                vec![],
                vec![make_proc(TaskId(99), "cmd")],
            ),
        );
        tasks.insert(
            TaskId(20),
            make_task_node(TaskId(20), "beta", Some(TaskId::ROOT), vec![], vec![]),
        );

        let snap = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks),
        };

        let rewritten = rewrite_snapshot(&snap, GenerationId(7));
        assert_eq!(rewritten.top_tasks.len(), 2);

        // Sorted by top id.
        let alpha = &rewritten.top_tasks[0];
        let beta = &rewritten.top_tasks[1];
        assert_eq!(alpha.id, "10");
        assert_eq!(alpha.parent, None);
        assert_eq!(alpha.gen_id, GenerationId(7));
        assert_eq!(beta.id, "20");

        // Sub-task id is "<top>.<sub>".
        assert_eq!(alpha.children.len(), 1);
        let sub = &alpha.children[0];
        assert_eq!(sub.id, "10.11");
        assert_eq!(sub.parent.as_deref(), Some("10"));

        // Process id is "<top>.<task>" (proc id 99 under top 10).
        assert_eq!(sub.processes.len(), 1);
        assert_eq!(sub.processes[0].id, "10.99");
    }

    #[test]
    fn rewrite_snapshot_top_id_is_bare() {
        // A lone top-level task should render as "<top>", not "<top>.<top>".
        let mut tasks = HashMap::new();
        tasks.insert(
            TaskId(5),
            make_task_node(TaskId(5), "solo", Some(TaskId::ROOT), vec![], vec![]),
        );
        let snap = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks),
        };
        let rewritten = rewrite_snapshot(&snap, GenerationId(1));
        assert_eq!(rewritten.top_tasks.len(), 1);
        assert_eq!(rewritten.top_tasks[0].id, "5");
    }

    // --- merge_snapshots ---

    #[test]
    fn merge_snapshots_orders_by_top_id_ascending() {
        let mut tasks_a = HashMap::new();
        tasks_a.insert(
            TaskId(30),
            make_task_node(TaskId(30), "a30", Some(TaskId::ROOT), vec![], vec![]),
        );
        tasks_a.insert(
            TaskId(10),
            make_task_node(TaskId(10), "a10", Some(TaskId::ROOT), vec![], vec![]),
        );
        let snap_a = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks_a),
        };
        let r_a = rewrite_snapshot(&snap_a, GenerationId(1));

        let mut tasks_b = HashMap::new();
        tasks_b.insert(
            TaskId(20),
            make_task_node(TaskId(20), "b20", Some(TaskId::ROOT), vec![], vec![]),
        );
        tasks_b.insert(
            TaskId(40),
            make_task_node(TaskId(40), "b40", Some(TaskId::ROOT), vec![], vec![]),
        );
        let snap_b = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(tasks_b),
        };
        let r_b = rewrite_snapshot(&snap_b, GenerationId(2));

        let merged = merge_snapshots(vec![r_a, r_b]);
        let ids: Vec<&str> = merged.top_tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["10", "20", "30", "40"]);
    }

    // --- resolve_address ---

    #[test]
    fn resolve_address_live_ok() {
        let mut m: EngineMap<&'static str> = EngineMap::new();
        m.insert(42, GenerationId(7), "g7");
        let (addr, r) = resolve_address("42.5.99", &m).unwrap();
        assert_eq!(addr.top, 42);
        assert_eq!(addr.task, 5);
        assert_eq!(addr.seq, Some(99));
        assert_eq!(r.gen_id, GenerationId(7));
    }

    #[test]
    fn resolve_address_retired_is_not_found() {
        let mut m: EngineMap<&'static str> = EngineMap::new();
        m.insert(42, GenerationId(7), "g7");
        m.retire_generation(GenerationId(7));
        let err = resolve_address("42", &m).unwrap_err();
        assert_eq!(err, ResolveError::NotFound(42));
    }

    #[test]
    fn resolve_address_unknown_is_not_found() {
        let m: EngineMap<&'static str> = EngineMap::new();
        let err = resolve_address("42", &m).unwrap_err();
        assert_eq!(err, ResolveError::NotFound(42));
    }

    #[test]
    fn resolve_address_malformed_is_bad_request() {
        let m: EngineMap<&'static str> = EngineMap::new();
        match resolve_address("t42", &m).unwrap_err() {
            ResolveError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }
}
