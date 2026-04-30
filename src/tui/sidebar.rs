//! Process sidebar widget for the TUI.
//!
//! Renders a sidebar showing tasks and their spawned processes with status
//! indicators. Entries are built from the engine's `GraphSnapshot` so the
//! TUI never duplicates lifecycle bookkeeping.
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
/// First entry is always the synthetic "All tasks" row whose source is
/// `snapshot.root` — selecting it focuses the root and drives the
/// unfiltered log view (design decision 2). Subsequent entries are
/// direct children of root with their processes and descendant tasks
/// nested below.
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

    // Synthetic "All tasks" row — static label per design decision 2:
    // no status tag, no count, no indent. The renderer treats it as
    // a heading (no visibility marker, blank row after it).
    entries.push(SidebarEntry {
        name: "All tasks".to_string(),
        source: snapshot.root,
        status_tag: String::new(),
        status_color: THEME.dim,
        visible: true,
        is_task: true,
        depth: 0,
    });

    // Direct children of root render flush left as peers of "All tasks"
    // (depth 0). The graph still parents them under root structurally;
    // the visual nesting is purely a sidebar choice. Descendant tasks
    // (`ctx.run` children) and their processes nest at depth+1.
    for &child_id in &root.children {
        push_task_subtree(
            snapshot,
            child_id,
            0,
            visible_sources,
            source_colors,
            &mut entries,
        );
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
        push_task_subtree(
            snapshot,
            child,
            depth + 1,
            visible_sources,
            source_colors,
            out,
        );
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
    // No header title — "All tasks" is the first entry and serves as
    // the sidebar's heading (decision: drop the multi-task-meaningless
    // "processes" label).
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

    let root_source = entries.first().map(|e| e.source);

    for (i, entry) in entries.iter().enumerate() {
        // `is_marked`: this index is the sidebar's selection — drives
        // the `>` arrow. Visible regardless of focus so the user sees
        // which entry drives the log filter even when the log pane has
        // focus.
        // `is_active`: full selection highlight (background) — only when
        // the sidebar itself has keyboard focus.
        let is_marked = i == state.selection;
        let is_active = state.focused && is_marked;
        let is_all_tasks_row = Some(entry.source) == root_source && i == 0;

        // "All tasks" is a heading: flush-left, no visibility marker,
        // followed by a blank row (decision: it's not a toggleable
        // source, just the root-focus shortcut).
        if is_all_tasks_row {
            let name_fg = if is_marked {
                // Same accent whether sidebar is active or just marked —
                // the bg fill (`is_active` patch below) carries the
                // additional "actively focused" hint.
                THEME.accent
            } else {
                THEME.dim
            };
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
            // Pad to width so selection highlight fills the row.
            let used = prefix.len() + entry.name.len();
            if used < inner.width as usize {
                spans.push(Span::raw(" ".repeat(inner.width as usize - used)));
            }
            let mut line = Line::from(spans);
            if is_active {
                line = line.patch_style(Style::default().bg(THEME.selection_bg));
            }
            lines.push(line);
            // Blank separator row before the actual tasks.
            lines.push(Line::from(""));
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

        if i > 0 && !entry.is_task {
            let prev = &entries[i - 1];
            let prev_is_running = prev.status_tag == "RUN" || prev.is_task;
            let curr_is_completed = entry.status_tag != "RUN";
            if prev_is_running && curr_is_completed && !entries[i - 1].is_task {
                lines.push(Line::from(""));
            }
        }

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
    fn build_entries_just_all_tasks_when_root_has_no_children() {
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
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "All tasks");
        assert_eq!(entries[0].source, TaskId::ROOT);
        assert_eq!(entries[0].depth, 0);
    }

    #[test]
    fn build_entries_single_task() {
        let mut sc = SourceColors::new();
        let id = TaskId(7);
        let snap = snap_with_one_task(id, "my-task", TaskStatus::Setup);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        // First entry is "All tasks", second is the actual task at depth 0
        // (peer of "All tasks" visually; the synthetic root header is
        // flush left and the task appears flush left below it).
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "All tasks");
        assert_eq!(entries[0].source, TaskId::ROOT);
        assert!(entries[1].is_task);
        assert_eq!(entries[1].name, "my-task");
        assert_eq!(entries[1].status_tag, "SETUP");
        assert_eq!(entries[1].source, id);
        assert_eq!(entries[1].depth, 0);
    }

    #[test]
    fn all_tasks_entry_is_first() {
        let mut sc = SourceColors::new();
        let id = TaskId(7);
        let snap = snap_with_one_task(id, "my-task", TaskStatus::Ready);
        let entries = build_sidebar_entries_from_graph(&snap, &HashSet::new(), &mut sc);
        assert_eq!(entries[0].source, TaskId::ROOT);
        assert_eq!(entries[0].name, "All tasks");
        assert!(entries[0].status_tag.is_empty());
        assert!(entries[0].is_task);
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
            is_task: true,
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
        // entries[0] is "All tasks" (always visible).
        // entries[1] is the actual task — should be dimmed.
        assert_eq!(entries.len(), 2);
        assert!(!entries[1].visible);
    }
}
