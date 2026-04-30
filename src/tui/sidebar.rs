//! Process sidebar widget for the TUI.
//!
//! Renders a sidebar showing tasks and their spawned processes with status
//! indicators. Entries are built from the engine's `GraphSnapshot` so the
//! TUI never duplicates lifecycle bookkeeping.
//!
//! See `docs/plans/notes/architecture.md` §5/§9.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::execution::{
    GraphSnapshot, ProcessStatus, TaskId, TaskNode, TaskStatus,
};
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

/// An entry in the sidebar's display list, derived from the engine's
/// `GraphSnapshot` at render time.
#[derive(Debug, Clone)]
pub struct SidebarEntry {
    /// Display name (task name or process command label).
    pub name: String,
    /// Source `TaskId` for log filtering (matches `LogEntry.source`).
    pub source: TaskId,
    /// Status tag text (e.g., "SETUP", "RUN", "DONE", "FAIL").
    pub status_tag: String,
    /// Color for the status tag.
    pub status_color: Color,
    /// Whether this source is currently visible in the log view.
    pub visible: bool,
    /// Whether this is the task entry (vs a process entry).
    pub is_task: bool,
    /// Nesting depth for indentation (0 = top-level task, 1 = process under task).
    pub depth: usize,
}

/// Build the list of sidebar entries from the engine's graph snapshot.
///
/// Lists every direct child of `TaskId::ROOT` as a top-level task, with its
/// processes nested under it (running first, then completed). The synthetic
/// root itself is not rendered.
pub fn build_sidebar_entries_from_graph(
    snapshot: &GraphSnapshot,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
) -> Vec<SidebarEntry> {
    let mut entries = Vec::new();

    // Walk children of root in deterministic order: order in `children`
    // reflects spawn order, which is what users expect.
    let Some(root) = snapshot.tasks.get(&snapshot.root) else {
        return entries;
    };

    // Recursive walk so descendant tasks (`ctx.run` children) appear under
    // their parent at depth+1, processes at depth+2. For now we keep depth
    // at 0/1 — multi-level nesting is a downstream UX pass.
    for &child_id in &root.children {
        push_task_subtree(snapshot, child_id, 0, visible_sources, source_colors, &mut entries);
    }

    entries
}

fn push_task_subtree(
    snapshot: &GraphSnapshot,
    id: TaskId,
    depth: usize,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
    out: &mut Vec<SidebarEntry>,
) {
    let Some(node) = snapshot.tasks.get(&id) else {
        return;
    };
    push_task_entry(node, depth, visible_sources, source_colors, out);
    push_process_entries(node, depth + 1, visible_sources, source_colors, out);
    for &child in &node.children {
        push_task_subtree(snapshot, child, depth + 1, visible_sources, source_colors, out);
    }
}

fn push_task_entry(
    node: &TaskNode,
    depth: usize,
    visible_sources: &HashSet<TaskId>,
    source_colors: &mut SourceColors,
    out: &mut Vec<SidebarEntry>,
) {
    let (tag, color) = task_status_display(&node.status);
    let _ = source_colors.color_for(node.id);
    out.push(SidebarEntry {
        name: node.name.clone(),
        source: node.id,
        status_tag: tag,
        status_color: color,
        visible: visible_sources.is_empty() || visible_sources.contains(&node.id),
        is_task: true,
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
    let mut running = Vec::new();
    let mut completed = Vec::new();
    for proc in &node.processes {
        match proc.status {
            ProcessStatus::Running => running.push(proc),
            _ => completed.push(proc),
        }
    }
    for proc in running {
        let _ = source_colors.color_for(proc.id);
        out.push(SidebarEntry {
            name: proc.command_label.clone(),
            source: proc.id,
            status_tag: "RUN".to_string(),
            status_color: THEME.status_running,
            visible: visible_sources.is_empty() || visible_sources.contains(&proc.id),
            is_task: false,
            depth,
        });
    }
    for proc in completed {
        let (tag, color) = process_status_display(&proc.status);
        let _ = source_colors.color_for(proc.id);
        out.push(SidebarEntry {
            name: proc.command_label.clone(),
            source: proc.id,
            status_tag: tag,
            status_color: color,
            visible: visible_sources.is_empty() || visible_sources.contains(&proc.id),
            is_task: false,
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
        .border_style(Style::default().fg(THEME.border))
        .title(Span::styled(
            " processes ",
            Style::default()
                .fg(if state.focused { THEME.accent } else { THEME.dim })
                .add_modifier(Modifier::BOLD),
        ));

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
        let is_selected = state.focused && i == state.selection;
        let indent_width = entry.depth * 2;
        let indent = " ".repeat(indent_width);
        let max_name_width =
            inner.width.saturating_sub(base_overhead + indent_width as u16) as usize;
        let prefix = if is_selected { "> " } else { "  " };
        let name_color = source_colors.color_for(entry.source);
        let name_style = if !entry.visible {
            Style::default().fg(THEME.dim)
        } else if entry.is_task {
            Style::default().fg(name_color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(name_color)
        };
        let display_name = if entry.name.len() > max_name_width {
            format!("{}~", &entry.name[..max_name_width.saturating_sub(1)])
        } else {
            entry.name.clone()
        };
        let tag = format!("[{}]", entry.status_tag);
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
            if is_selected {
                Style::default().fg(THEME.accent)
            } else {
                Style::default().fg(THEME.dim)
            },
        ));
        if indent_width > 0 {
            spans.push(Span::raw(indent));
        }
        spans.push(Span::styled(format!("{} ", visibility_marker), marker_style));
        spans.push(Span::styled(display_name, name_style));
        spans.push(Span::raw(padding));
        spans.push(Span::styled(tag, tag_style));

        if i > 0 && !entry.is_task {
            let prev = &entries[i - 1];
            let prev_is_running = prev.status_tag == "RUN" || prev.is_task;
            let curr_is_completed = entry.status_tag != "RUN";
            if prev_is_running && curr_is_completed && !entries[i - 1].is_task {
                lines.push(Line::from(""));
            }
        }

        let mut line = Line::from(spans);
        if is_selected {
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
    fn build_entries_empty() {
        let mut sc = SourceColors::new();
        let snap = GraphSnapshot::default();
        let entries =
            build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        assert!(entries.is_empty());
    }

    #[test]
    fn build_entries_single_task() {
        let mut sc = SourceColors::new();
        let id = TaskId(7);
        let snap = snap_with_one_task(id, "my-task", TaskStatus::Setup);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_task);
        assert_eq!(entries[0].name, "my-task");
        assert_eq!(entries[0].status_tag, "SETUP");
        assert_eq!(entries[0].source, id);
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
        let (tag, color) = task_status_display(&TaskStatus::Failed(
            crate::execution::TaskFailure {
                message: "err".to_string(),
                exit_code: 1,
                output_json: "{}".to_string(),
            },
        ));
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
        let entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: id,
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
        ];
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
        assert!(!entries[0].visible);
    }
}
