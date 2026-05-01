//! Process sidebar widget for the TUI.
//!
//! Renders a sidebar showing tasks and their spawned processes with status
//! indicators. Entries are built from the engine's `GraphSnapshot` so the
//! TUI never duplicates lifecycle bookkeeping.
//!
//! Layout has three sections:
//!
//! ```text
//! > All tasks
//!
//!   Running tasks
//!     web                [RUN]
//!       cargo run        [RUN]
//!     api                [READY]
//!
//!   Completed tasks
//!     [42] web           [DONE]
//!       cargo run        [DONE]
//!     [37] api           [FAIL]
//! ```
//!
//! Each section header ("All tasks", "Running tasks", "Completed tasks")
//! is itself a selectable entry that drives a focus filter (everything,
//! all running top-level subtrees, or all completed top-level subtrees).
//!
//! See `docs/runtime_engine_design.md` § Graph snapshot & observation and § Logging.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::execution::{GraphSnapshot, ProcessStatus, TaskId, TaskNode, TaskStatus};
use crate::theme::{SourceColors, THEME};

/// Fixed sidebar width in columns.
pub const SIDEBAR_WIDTH: u16 = 28;

/// State for the sidebar.
#[derive(Debug, Clone)]
pub struct SidebarState {
    /// Whether the sidebar has keyboard focus.
    pub focused: bool,
    /// Index of the selected entry in the sidebar list.
    pub selection: usize,
}

impl SidebarState {
    pub fn new() -> Self {
        Self {
            focused: false,
            selection: 0,
        }
    }

    pub fn move_up(&mut self) {
        self.selection = self.selection.saturating_sub(1);
    }

    pub fn move_down(&mut self, max_entries: usize) {
        if max_entries > 0 {
            self.selection = (self.selection + 1).min(max_entries - 1);
        }
    }

    pub fn clamp_selection(&mut self, max_entries: usize) {
        if max_entries == 0 {
            self.selection = 0;
        } else {
            self.selection = self.selection.min(max_entries - 1);
        }
    }
}

impl Default for SidebarState {
    fn default() -> Self {
        Self::new()
    }
}

/// Discriminator for what kind of row a sidebar entry represents.
///
/// - `AllTasks` — synthetic root header; selecting clears the focus filter.
/// - `RunningHeader` — selecting filters log to all currently running
///   top-level tasks (and their descendants/processes).
/// - `CompletedHeader` — selecting filters log to all completed top-level
///   tasks (and their descendants/processes).
/// - `Task` — a real task in the graph. Top-level entries in the
///   *Completed* section have their display name prefixed with `[id]`.
/// - `Process` — a spawned process row, nested under its owning task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarEntryKind {
    AllTasks,
    RunningHeader,
    CompletedHeader,
    Task,
    Process,
}

impl SidebarEntryKind {
    /// Whether this kind is one of the section headers (which act as
    /// filter-driving rows but aren't themselves toggleable/signalable).
    pub fn is_section_header(self) -> bool {
        matches!(
            self,
            SidebarEntryKind::AllTasks
                | SidebarEntryKind::RunningHeader
                | SidebarEntryKind::CompletedHeader
        )
    }
}

/// An entry in the sidebar's display list, derived from the engine's
/// `GraphSnapshot` at render time.
#[derive(Debug, Clone)]
pub struct SidebarEntry {
    /// Display name (task name or process command label).
    pub name: String,
    /// Source `TaskId` for log filtering (matches `LogEntry.source`).
    /// Section headers carry `TaskId::ROOT` (no specific source).
    pub source: TaskId,
    /// Status tag text (e.g., "SETUP", "RUN", "DONE", "FAIL").
    pub status_tag: String,
    /// Color for the status tag.
    pub status_color: Color,
    /// Whether this source is currently visible in the log view.
    pub visible: bool,
    /// What kind of entry this is (drives selection / filter behavior).
    pub kind: SidebarEntryKind,
    /// Nesting depth for indentation (0 = top-level task, 1 = process under task).
    pub depth: usize,
}

impl SidebarEntry {
    /// Whether this entry corresponds to a real task in the graph (vs a
    /// process row or a section header). Convenience for code paths that
    /// previously read the boolean `is_task` field directly.
    pub fn is_task(&self) -> bool {
        matches!(self.kind, SidebarEntryKind::Task)
    }

    /// Whether this entry is a section header — drives the no-op guards
    /// in keys.rs and the special branching in `refresh_focus_filter`.
    pub fn is_section_header(&self) -> bool {
        self.kind.is_section_header()
    }

    /// Whether this entry is a "section-level" landmark — section headers
    /// or top-level task rows. These are the jump targets for `[` / `]`
    /// navigation; sub-tasks and processes are skipped.
    pub fn is_section_level(&self) -> bool {
        self.is_section_header() || (matches!(self.kind, SidebarEntryKind::Task) && self.depth == 0)
    }
}

/// Find the next section-level entry strictly after `from`, or `None` if
/// `from` is already at the last section-level row.
pub fn next_section_level(entries: &[SidebarEntry], from: usize) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .skip(from + 1)
        .find(|(_, e)| e.is_section_level())
        .map(|(i, _)| i)
}

/// Find the next section-level entry strictly before `from`, or `None` if
/// `from` is already at or above the first section-level row.
pub fn prev_section_level(entries: &[SidebarEntry], from: usize) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .take(from)
        .rev()
        .find(|(_, e)| e.is_section_level())
        .map(|(i, _)| i)
}

/// Build the list of sidebar entries from the engine's graph snapshot.
///
/// Layout:
///
/// 1. "All tasks" header (always present).
/// 2. Blank separator + "Running tasks" header + each running top-level
///    task's subtree (processes + descendant tasks).
/// 3. Blank separator + "Completed tasks" header + each completed top-level
///    task's subtree, ordered newest-first by `TaskId`. Top-level entries
///    here get a `[id]` prefix on their displayed name.
///
/// "Running" iff the task's status is `Setup | Ready`; everything else
/// counts as "completed" (`Done`, `Failed`, `Cancelled`, `Timeout`).
///
/// Blank separator rows are encoded by inserting `None`-shaped rendering
/// in the renderer rather than as entries here — entries returned from
/// this function are all real, selectable rows.
///
/// `visible_sources` is the *effective* set after composing focus +
/// hidden manual toggles — used to dim entries hidden from the log view.
pub fn build_sidebar_entries_from_graph(
    snapshot: &GraphSnapshot,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
) -> Vec<SidebarEntry> {
    let mut entries = Vec::new();

    let Some(root) = snapshot.tasks.get(&snapshot.root) else {
        return entries;
    };

    // 1. "All tasks" header.
    entries.push(SidebarEntry {
        name: "All tasks".to_string(),
        source: snapshot.root,
        status_tag: String::new(),
        status_color: THEME.dim,
        visible: true,
        kind: SidebarEntryKind::AllTasks,
        depth: 0,
    });

    // Partition top-level (direct children of root) tasks into running vs
    // completed, preserving the graph's existing order for running and
    // sorting completed newest-first.
    let mut running_top: Vec<TaskId> = Vec::new();
    let mut completed_top: Vec<TaskId> = Vec::new();
    for &child_id in &root.children {
        let Some(child) = snapshot.tasks.get(&child_id) else {
            continue;
        };
        if is_running_status(&child.status) {
            running_top.push(child_id);
        } else {
            completed_top.push(child_id);
        }
    }
    // Newest-first within Completed (highest TaskId first).
    completed_top.sort_by(|a, b| b.0.cmp(&a.0));

    // 2. Running section.
    entries.push(SidebarEntry {
        name: "Running tasks".to_string(),
        source: snapshot.root,
        status_tag: String::new(),
        status_color: THEME.dim,
        visible: true,
        kind: SidebarEntryKind::RunningHeader,
        depth: 0,
    });
    for &child_id in &running_top {
        push_task_subtree(
            snapshot,
            child_id,
            0,
            /* prefix_id */ false,
            visible_sources,
            source_colors,
            &mut entries,
        );
    }

    // 3. Completed section.
    entries.push(SidebarEntry {
        name: "Completed tasks".to_string(),
        source: snapshot.root,
        status_tag: String::new(),
        status_color: THEME.dim,
        visible: true,
        kind: SidebarEntryKind::CompletedHeader,
        depth: 0,
    });
    for &child_id in &completed_top {
        push_task_subtree(
            snapshot,
            child_id,
            0,
            /* prefix_id */ true,
            visible_sources,
            source_colors,
            &mut entries,
        );
    }

    entries
}

/// "Running" iff the task is `Setup` or `Ready`. Everything else (Done,
/// Failed, Cancelled, Timeout) is "completed" for sidebar partitioning.
fn is_running_status(status: &TaskStatus) -> bool {
    matches!(status, TaskStatus::Setup | TaskStatus::Ready)
}

fn push_task_subtree(
    snapshot: &GraphSnapshot,
    id: TaskId,
    depth: usize,
    prefix_id: bool,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
    out: &mut Vec<SidebarEntry>,
) {
    let Some(node) = snapshot.tasks.get(&id) else {
        return;
    };
    push_task_entry(node, depth, prefix_id, visible_sources, source_colors, out);
    push_process_entries(node, depth + 1, visible_sources, source_colors, out);
    // Sub-tasks/processes never get the [id] prefix — only the top-level
    // entry of a Completed subtree does.
    for &child in &node.children {
        push_task_subtree(
            snapshot,
            child,
            depth + 1,
            /* prefix_id */ false,
            visible_sources,
            source_colors,
            out,
        );
    }
}

fn push_task_entry(
    node: &TaskNode,
    depth: usize,
    prefix_id: bool,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
    out: &mut Vec<SidebarEntry>,
) {
    let (tag, color) = task_status_display(&node.status);
    let _ = source_colors.color_for(node.id);
    let display_name = if prefix_id {
        format!("[{}] {}", node.id.0, node.name)
    } else {
        node.name.clone()
    };
    out.push(SidebarEntry {
        name: display_name,
        source: node.id,
        status_tag: tag,
        status_color: color,
        visible: visible_sources.is_empty() || visible_sources.contains(&node.id),
        kind: SidebarEntryKind::Task,
        depth,
    });
}

fn push_process_entries(
    node: &TaskNode,
    depth: usize,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
    out: &mut Vec<SidebarEntry>,
) {
    for proc in &node.processes {
        let (tag, color) = match proc.status {
            ProcessStatus::Running => ("RUN".to_string(), THEME.status_running),
            _ => process_status_display(&proc.status),
        };
        let _ = source_colors.color_for(proc.id);
        out.push(SidebarEntry {
            name: proc.command_label.clone(),
            source: proc.id,
            status_tag: tag,
            status_color: color,
            visible: visible_sources.is_empty() || visible_sources.contains(&proc.id),
            kind: SidebarEntryKind::Process,
            depth,
        });
    }
}

/// Render the sidebar into the given area.
pub fn render_sidebar(
    frame: &mut Frame,
    area: Rect,
    entries: &[SidebarEntry],
    state: &SidebarState,
    source_colors: &mut SourceColors,
) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(THEME.border));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if entries.is_empty() {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "  No task running",
            Style::default().fg(THEME.dim),
        )));
        frame.render_widget(placeholder, inner);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let base_overhead = 12_u16;

    for (i, entry) in entries.iter().enumerate() {
        // `is_marked`: this index is the sidebar's selection — drives
        // the `>` arrow. Visible regardless of focus so the user sees
        // which entry drives the log filter even when the log pane has
        // focus.
        // `is_active`: full selection highlight (background) — only when
        // the sidebar itself has keyboard focus.
        let is_marked = i == state.selection;
        let is_active = state.focused && is_marked;

        // Section headers get heading-style rendering: flush-left, no
        // visibility marker. Each section header (other than the very
        // first "All tasks" row) is preceded by a blank separator row.
        if entry.is_section_header() {
            if !matches!(entry.kind, SidebarEntryKind::AllTasks) {
                lines.push(Line::from(""));
            }

            let name_fg = if is_marked { THEME.accent } else { THEME.dim };
            let name_style = Style::default().fg(name_fg).add_modifier(Modifier::BOLD);
            let prefix = if is_marked { "> " } else { "  " };
            let prefix_style = if is_marked {
                Style::default().fg(THEME.accent)
            } else {
                Style::default().fg(THEME.dim)
            };

            let mut spans = vec![
                Span::styled(prefix.to_string(), prefix_style),
                Span::styled(entry.name.clone(), name_style),
            ];
            // Pad to width so the selection highlight fills the row.
            let used = prefix.len() + entry.name.len();
            if used < inner.width as usize {
                spans.push(Span::raw(" ".repeat(inner.width as usize - used)));
            }
            let mut line = Line::from(spans);
            if is_active {
                line = line.patch_style(Style::default().bg(THEME.selection_bg));
            }
            lines.push(line);
            continue;
        }

        let indent_width = entry.depth * 2;
        let indent = " ".repeat(indent_width);
        let max_name_width = inner
            .width
            .saturating_sub(base_overhead + indent_width as u16)
            as usize;
        let prefix = if is_marked { "> " } else { "  " };
        let name_color = source_colors.color_for(entry.source);
        let name_style = if !entry.visible {
            Style::default().fg(THEME.dim)
        } else if entry.is_task() {
            Style::default().fg(name_color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(name_color)
        };
        let display_name = if entry.name.len() > max_name_width {
            format!("{}~", &entry.name[..max_name_width.saturating_sub(1)])
        } else {
            entry.name.clone()
        };
        let tag = if entry.status_tag.is_empty() {
            String::new()
        } else {
            format!("[{}]", entry.status_tag)
        };
        let used = prefix.len() + indent_width + 2 + display_name.len() + tag.len();
        let padding = if used < inner.width as usize {
            " ".repeat(inner.width as usize - used)
        } else {
            " ".to_string()
        };
        let tag_style = if !entry.visible {
            Style::default().fg(THEME.dim)
        } else {
            Style::default().fg(entry.status_color)
        };
        let visibility_marker = if !entry.visible { "-" } else { "*" };
        let marker_style = if !entry.visible {
            Style::default().fg(THEME.dim)
        } else {
            Style::default().fg(name_color)
        };

        let mut spans = Vec::new();
        spans.push(Span::styled(
            prefix.to_string(),
            if is_marked {
                Style::default().fg(THEME.accent)
            } else {
                Style::default().fg(THEME.dim)
            },
        ));
        if indent_width > 0 {
            spans.push(Span::raw(indent));
        }
        spans.push(Span::styled(
            format!("{} ", visibility_marker),
            marker_style,
        ));
        spans.push(Span::styled(display_name, name_style));
        spans.push(Span::raw(padding));
        spans.push(Span::styled(tag, tag_style));

        let mut line = Line::from(spans);
        if is_active {
            line = line.patch_style(Style::default().bg(THEME.selection_bg));
        }
        lines.push(line);
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Get display tag and color for a `TaskStatus`.
pub fn task_status_display(status: &TaskStatus) -> (String, Color) {
    match status {
        TaskStatus::Setup => ("SETUP".to_string(), THEME.status_setup),
        TaskStatus::Ready => ("READY".to_string(), THEME.status_running),
        TaskStatus::Done => ("DONE".to_string(), THEME.status_done),
        TaskStatus::Failed(_) => ("FAIL".to_string(), THEME.status_failed),
        TaskStatus::Cancelled => ("CANCEL".to_string(), THEME.status_failed),
        TaskStatus::Timeout => ("TIMEOUT".to_string(), THEME.status_failed),
    }
}

/// Get display tag and color for a `ProcessStatus`.
fn process_status_display(status: &ProcessStatus) -> (String, Color) {
    use crate::process::Termination;
    match status {
        ProcessStatus::Running => ("RUN".to_string(), THEME.status_running),
        ProcessStatus::Done => ("DONE".to_string(), THEME.status_done),
        ProcessStatus::Failed(Termination::Exited(code)) => {
            (format!("FAIL:{}", code), THEME.status_failed)
        }
        ProcessStatus::Failed(Termination::Signaled(sig)) => {
            (format!("SIG:{}", sig), THEME.status_failed)
        }
        ProcessStatus::Failed(Termination::TimedOut) => {
            ("TIMEOUT".to_string(), THEME.status_failed)
        }
        ProcessStatus::Stopped => ("STOP".to_string(), THEME.status_stopped),
    }
}

/// Get the source `TaskId` for the Nth sidebar entry (0-indexed).
pub fn source_for_index(entries: &[SidebarEntry], index: usize) -> Option<TaskId> {
    entries.get(index).map(|e| e.source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn snap_with_one_task(id: TaskId, name: &str, status: TaskStatus) -> GraphSnapshot {
        let root = TaskNode {
            id: TaskId::ROOT,
            name: "<root>".to_string(),
            parent: None,
            children: vec![id],
            status: TaskStatus::Setup,
            processes: Vec::new(),
        };
        let task = TaskNode {
            id,
            name: name.to_string(),
            parent: Some(TaskId::ROOT),
            children: Vec::new(),
            status,
            processes: Vec::new(),
        };
        let mut map = std::collections::HashMap::new();
        map.insert(TaskId::ROOT, root);
        map.insert(id, task);
        GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(map),
        }
    }

    fn snap_with_tasks(specs: &[(TaskId, &str, TaskStatus)]) -> GraphSnapshot {
        let mut children = Vec::new();
        let mut map = std::collections::HashMap::new();
        for (id, name, status) in specs {
            children.push(*id);
            map.insert(
                *id,
                TaskNode {
                    id: *id,
                    name: (*name).to_string(),
                    parent: Some(TaskId::ROOT),
                    children: Vec::new(),
                    status: status.clone(),
                    processes: Vec::new(),
                },
            );
        }
        let root = TaskNode {
            id: TaskId::ROOT,
            name: "<root>".to_string(),
            parent: None,
            children,
            status: TaskStatus::Setup,
            processes: Vec::new(),
        };
        map.insert(TaskId::ROOT, root);
        GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(map),
        }
    }

    /// Helper: locate the index of the first entry of a given kind.
    fn idx_of_kind(entries: &[SidebarEntry], kind: SidebarEntryKind) -> Option<usize> {
        entries.iter().position(|e| e.kind == kind)
    }

    #[test]
    fn sidebar_state_defaults() {
        let state = SidebarState::new();
        assert!(!state.focused);
        assert_eq!(state.selection, 0);
    }

    #[test]
    fn sidebar_state_move_up_clamps() {
        let mut state = SidebarState::new();
        state.move_up();
        assert_eq!(state.selection, 0);
    }

    #[test]
    fn sidebar_state_move_down_clamps() {
        let mut state = SidebarState::new();
        state.move_down(3);
        assert_eq!(state.selection, 1);
        state.move_down(3);
        assert_eq!(state.selection, 2);
        state.move_down(3);
        assert_eq!(state.selection, 2);
    }

    #[test]
    fn sidebar_state_clamp_selection() {
        let mut state = SidebarState::new();
        state.selection = 10;
        state.clamp_selection(3);
        assert_eq!(state.selection, 2);
    }

    #[test]
    fn build_entries_empty_when_no_root() {
        let mut sc = SourceColors::new();
        let snap = GraphSnapshot::default();
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        // Default snapshot has no root entry => no entries at all.
        assert!(entries.is_empty());
    }

    #[test]
    fn build_entries_just_section_headers_when_root_has_no_children() {
        let mut sc = SourceColors::new();
        let root = TaskNode {
            id: TaskId::ROOT,
            name: "<root>".to_string(),
            parent: None,
            children: Vec::new(),
            status: TaskStatus::Setup,
            processes: Vec::new(),
        };
        let mut map = std::collections::HashMap::new();
        map.insert(TaskId::ROOT, root);
        let snap = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(map),
        };
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        // Three headers always present once there is a root.
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, SidebarEntryKind::AllTasks);
        assert_eq!(entries[1].kind, SidebarEntryKind::RunningHeader);
        assert_eq!(entries[2].kind, SidebarEntryKind::CompletedHeader);
    }

    #[test]
    fn build_entries_single_running_task() {
        let mut sc = SourceColors::new();
        let id = TaskId(7);
        let snap = snap_with_one_task(id, "my-task", TaskStatus::Setup);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        // [AllTasks, RunningHeader, Task, CompletedHeader].
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].kind, SidebarEntryKind::AllTasks);
        assert_eq!(entries[1].kind, SidebarEntryKind::RunningHeader);
        assert_eq!(entries[2].kind, SidebarEntryKind::Task);
        assert_eq!(entries[2].name, "my-task");
        assert!(!entries[2].name.starts_with('[')); // running entries don't get [id] prefix
        assert_eq!(entries[2].status_tag, "SETUP");
        assert_eq!(entries[2].source, id);
        assert_eq!(entries[3].kind, SidebarEntryKind::CompletedHeader);
    }

    #[test]
    fn completed_top_level_gets_id_prefix() {
        let mut sc = SourceColors::new();
        let id = TaskId(42);
        let snap = snap_with_one_task(id, "web", TaskStatus::Done);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        // [AllTasks, RunningHeader, CompletedHeader, Task].
        assert_eq!(entries.len(), 4);
        let task_idx = idx_of_kind(&entries, SidebarEntryKind::Task).unwrap();
        // Task lives below the CompletedHeader in this case.
        assert!(task_idx > idx_of_kind(&entries, SidebarEntryKind::CompletedHeader).unwrap());
        assert_eq!(entries[task_idx].name, "[42] web");
    }

    #[test]
    fn running_top_level_no_id_prefix() {
        let mut sc = SourceColors::new();
        let id = TaskId(11);
        let snap = snap_with_one_task(id, "api", TaskStatus::Ready);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        let task_idx = idx_of_kind(&entries, SidebarEntryKind::Task).unwrap();
        assert_eq!(entries[task_idx].name, "api");
    }

    #[test]
    fn completed_ordered_newest_first() {
        // Three completed top-level tasks with ids 10, 22, 5; expect 22, 10, 5.
        let mut sc = SourceColors::new();
        let snap = snap_with_tasks(&[
            (TaskId(10), "alpha", TaskStatus::Done),
            (TaskId(22), "beta", TaskStatus::Done),
            (TaskId(5), "gamma", TaskStatus::Done),
        ]);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        let completed_idx = idx_of_kind(&entries, SidebarEntryKind::CompletedHeader).unwrap();
        // Tasks following the CompletedHeader, in order.
        let names: Vec<&str> = entries[completed_idx + 1..]
            .iter()
            .filter(|e| e.kind == SidebarEntryKind::Task)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["[22] beta", "[10] alpha", "[5] gamma"]);
    }

    #[test]
    fn running_completed_split_partitions_correctly() {
        let mut sc = SourceColors::new();
        let snap = snap_with_tasks(&[
            (TaskId(1), "running-setup", TaskStatus::Setup),
            (TaskId(2), "completed-done", TaskStatus::Done),
            (TaskId(3), "running-ready", TaskStatus::Ready),
            (
                TaskId(4),
                "completed-failed",
                TaskStatus::Failed(crate::execution::TaskFailure {
                    message: "x".to_string(),
                    exit_code: 1,
                    output_json: "{}".to_string(),
                }),
            ),
            (TaskId(5), "completed-cancelled", TaskStatus::Cancelled),
            (TaskId(6), "completed-timeout", TaskStatus::Timeout),
        ]);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        let running_hdr = idx_of_kind(&entries, SidebarEntryKind::RunningHeader).unwrap();
        let completed_hdr = idx_of_kind(&entries, SidebarEntryKind::CompletedHeader).unwrap();

        let between_running: Vec<&str> = entries[running_hdr + 1..completed_hdr]
            .iter()
            .filter(|e| e.kind == SidebarEntryKind::Task)
            .map(|e| e.name.as_str())
            .collect();
        let after_completed: Vec<&str> = entries[completed_hdr + 1..]
            .iter()
            .filter(|e| e.kind == SidebarEntryKind::Task)
            .map(|e| e.name.as_str())
            .collect();

        // Running stays in graph order (Setup, Ready preserved as 1, 3).
        assert_eq!(between_running, vec!["running-setup", "running-ready"]);
        // Completed sorted newest-first: ids 6, 5, 4, 2.
        assert_eq!(
            after_completed,
            vec![
                "[6] completed-timeout",
                "[5] completed-cancelled",
                "[4] completed-failed",
                "[2] completed-done",
            ]
        );
    }

    #[test]
    fn section_headers_are_selectable() {
        let mut sc = SourceColors::new();
        let id = TaskId(7);
        let snap = snap_with_one_task(id, "my-task", TaskStatus::Setup);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        // "Selectable" here means: section headers are full entries in the
        // returned vector (not skipped), so SidebarState selection can land
        // on them and key handlers will see them via index lookup.
        for kind in [
            SidebarEntryKind::AllTasks,
            SidebarEntryKind::RunningHeader,
            SidebarEntryKind::CompletedHeader,
        ] {
            let idx = idx_of_kind(&entries, kind);
            assert!(idx.is_some(), "expected to find {:?}", kind);
            let e = &entries[idx.unwrap()];
            assert!(e.is_section_header());
            // Section headers carry the root TaskId as a placeholder source.
            assert_eq!(e.source, TaskId::ROOT);
        }
    }

    #[test]
    fn subtree_migrates_with_top_level_entry() {
        // A top-level task with a sub-task and a process. When the parent
        // is Done, the entire subtree should appear under Completed.
        let mut sc = SourceColors::new();
        let parent_id = TaskId(20);
        let child_id = TaskId(21);
        let proc_id = TaskId(22);

        let mut map = std::collections::HashMap::new();
        let parent = TaskNode {
            id: parent_id,
            name: "web".to_string(),
            parent: Some(TaskId::ROOT),
            children: vec![child_id],
            status: TaskStatus::Done,
            processes: vec![crate::execution::ProcessNodeInfo {
                id: proc_id,
                task_name: "web".to_string(),
                command_label: "cargo run".to_string(),
                pid: None,
                pgid: None,
                status: ProcessStatus::Done,
                ready: false,
            }],
        };
        let child = TaskNode {
            id: child_id,
            name: "subtask".to_string(),
            parent: Some(parent_id),
            children: Vec::new(),
            status: TaskStatus::Done,
            processes: Vec::new(),
        };
        let root = TaskNode {
            id: TaskId::ROOT,
            name: "<root>".to_string(),
            parent: None,
            children: vec![parent_id],
            status: TaskStatus::Setup,
            processes: Vec::new(),
        };
        map.insert(TaskId::ROOT, root);
        map.insert(parent_id, parent);
        map.insert(child_id, child);
        let snap = GraphSnapshot {
            root: TaskId::ROOT,
            tasks: Arc::new(map),
        };
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        let completed_hdr = idx_of_kind(&entries, SidebarEntryKind::CompletedHeader).unwrap();
        let after: Vec<(SidebarEntryKind, &str, usize)> = entries[completed_hdr + 1..]
            .iter()
            .map(|e| (e.kind, e.name.as_str(), e.depth))
            .collect();
        // Expect: [parent task with [id] prefix, its process, its sub-task].
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], (SidebarEntryKind::Task, "[20] web", 0));
        assert_eq!(after[1], (SidebarEntryKind::Process, "cargo run", 1));
        // Sub-task does not get an [id] prefix.
        assert_eq!(after[2], (SidebarEntryKind::Task, "subtask", 1));
    }

    #[test]
    fn task_status_display_all_variants() {
        assert_eq!(
            task_status_display(&TaskStatus::Setup),
            ("SETUP".to_string(), Color::Yellow)
        );
        assert_eq!(
            task_status_display(&TaskStatus::Ready),
            ("READY".to_string(), Color::Green)
        );
        assert_eq!(
            task_status_display(&TaskStatus::Done),
            ("DONE".to_string(), Color::DarkGray)
        );
        let (tag, color) =
            task_status_display(&TaskStatus::Failed(crate::execution::TaskFailure {
                message: "err".to_string(),
                exit_code: 1,
                output_json: "{}".to_string(),
            }));
        assert_eq!(tag, "FAIL");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn process_status_display_all_variants() {
        assert_eq!(
            process_status_display(&ProcessStatus::Running),
            ("RUN".to_string(), Color::Green)
        );
        assert_eq!(
            process_status_display(&ProcessStatus::Done),
            ("DONE".to_string(), Color::DarkGray)
        );
        let (tag, color) = process_status_display(&ProcessStatus::Failed(
            crate::process::Termination::Exited(1),
        ));
        assert_eq!(tag, "FAIL:1");
        assert_eq!(color, Color::Red);
        assert_eq!(
            process_status_display(&ProcessStatus::Stopped),
            ("STOP".to_string(), Color::Yellow)
        );
    }

    #[test]
    fn source_for_index_valid() {
        let id = TaskId(7);
        let entries = vec![SidebarEntry {
            name: "task".to_string(),
            source: id,
            status_tag: "SETUP".to_string(),
            status_color: Color::Yellow,
            visible: true,
            kind: SidebarEntryKind::Task,
            depth: 0,
        }];
        assert_eq!(source_for_index(&entries, 0), Some(id));
        assert_eq!(source_for_index(&entries, 1), None);
    }

    #[test]
    fn build_entries_visibility_with_filter() {
        let mut sc = SourceColors::new();
        let id = TaskId(7);
        let snap = snap_with_one_task(id, "my-task", TaskStatus::Ready);
        // visible_sources contains a different id, so this task is dimmed.
        let mut visible = HashSet::new();
        visible.insert(TaskId(99));
        let entries = build_sidebar_entries_from_graph(&snap, &visible, &mut sc);
        let task_idx = idx_of_kind(&entries, SidebarEntryKind::Task).unwrap();
        assert!(!entries[task_idx].visible);
    }

    #[test]
    fn is_section_header_method() {
        let header = SidebarEntry {
            name: "Running tasks".to_string(),
            source: TaskId::ROOT,
            status_tag: String::new(),
            status_color: Color::Gray,
            visible: true,
            kind: SidebarEntryKind::RunningHeader,
            depth: 0,
        };
        assert!(header.is_section_header());
        assert!(!header.is_task());

        let task = SidebarEntry {
            name: "x".to_string(),
            source: TaskId(1),
            status_tag: "RUN".to_string(),
            status_color: Color::Green,
            visible: true,
            kind: SidebarEntryKind::Task,
            depth: 0,
        };
        assert!(!task.is_section_header());
        assert!(task.is_task());
    }

    fn make_entry(kind: SidebarEntryKind, depth: usize, source: TaskId) -> SidebarEntry {
        SidebarEntry {
            name: String::new(),
            source,
            status_tag: String::new(),
            status_color: Color::Gray,
            visible: true,
            kind,
            depth,
        }
    }

    #[test]
    fn section_level_includes_headers_and_top_level_tasks() {
        let entries = vec![
            make_entry(SidebarEntryKind::AllTasks, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::RunningHeader, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::Task, 0, TaskId(1)),
            make_entry(SidebarEntryKind::Process, 1, TaskId(2)),
            make_entry(SidebarEntryKind::Task, 1, TaskId(3)), // sub-task
            make_entry(SidebarEntryKind::CompletedHeader, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::Task, 0, TaskId(4)),
        ];
        let levels: Vec<bool> = entries.iter().map(|e| e.is_section_level()).collect();
        assert_eq!(levels, vec![true, true, true, false, false, true, true]);
    }

    #[test]
    fn next_section_level_skips_subitems() {
        let entries = vec![
            make_entry(SidebarEntryKind::AllTasks, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::RunningHeader, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::Task, 0, TaskId(1)),
            make_entry(SidebarEntryKind::Process, 1, TaskId(2)),
            make_entry(SidebarEntryKind::Task, 1, TaskId(3)),
            make_entry(SidebarEntryKind::CompletedHeader, 0, TaskId::ROOT),
        ];
        // From "All tasks" → next is RunningHeader.
        assert_eq!(next_section_level(&entries, 0), Some(1));
        // From RunningHeader → next is the top-level Task.
        assert_eq!(next_section_level(&entries, 1), Some(2));
        // From the top-level Task → next jumps over process + sub-task.
        assert_eq!(next_section_level(&entries, 2), Some(5));
        // From a sub-item → finds the next section-level row going down.
        assert_eq!(next_section_level(&entries, 3), Some(5));
        // At the last section-level row → None.
        assert_eq!(next_section_level(&entries, 5), None);
    }

    #[test]
    fn prev_section_level_skips_subitems() {
        let entries = vec![
            make_entry(SidebarEntryKind::AllTasks, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::RunningHeader, 0, TaskId::ROOT),
            make_entry(SidebarEntryKind::Task, 0, TaskId(1)),
            make_entry(SidebarEntryKind::Process, 1, TaskId(2)),
            make_entry(SidebarEntryKind::Task, 1, TaskId(3)),
            make_entry(SidebarEntryKind::CompletedHeader, 0, TaskId::ROOT),
        ];
        // From CompletedHeader → previous is the top-level Task.
        assert_eq!(prev_section_level(&entries, 5), Some(2));
        // From a sub-item → finds the next section-level row going up.
        assert_eq!(prev_section_level(&entries, 4), Some(2));
        // From the top-level Task → RunningHeader.
        assert_eq!(prev_section_level(&entries, 2), Some(1));
        // At the first row → None.
        assert_eq!(prev_section_level(&entries, 0), None);
    }
}
