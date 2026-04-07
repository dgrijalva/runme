//! Process sidebar widget for the TUI.
//!
//! Renders a sidebar showing the task and its spawned processes with status
//! indicators. Three sections: Task (top), Running processes, Completed processes.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::render::SourceColors;
use super::runner::{ProcessInfo, ProcessStatus, TaskStatus};

/// Fixed sidebar width in columns.
pub const SIDEBAR_WIDTH: u16 = 26;

/// State for the sidebar.
#[derive(Debug, Clone)]
pub struct SidebarState {
    /// Whether the sidebar has keyboard focus.
    pub focused: bool,
    /// Index of the selected entry in the sidebar list.
    /// 0 = the task itself, 1..N = processes in display order.
    pub selection: usize,
}

impl SidebarState {
    pub fn new() -> Self {
        Self {
            focused: false,
            selection: 0,
        }
    }

    /// Move selection up, clamping at 0.
    pub fn move_up(&mut self) {
        self.selection = self.selection.saturating_sub(1);
    }

    /// Move selection down, clamping at max_entries - 1.
    pub fn move_down(&mut self, max_entries: usize) {
        if max_entries > 0 {
            self.selection = (self.selection + 1).min(max_entries - 1);
        }
    }

    /// Clamp selection to valid range.
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

/// An entry in the sidebar's display list. This is a snapshot taken at render
/// time from the runner's process list.
#[derive(Debug, Clone)]
pub struct SidebarEntry {
    /// Display name (task name or command label).
    pub name: String,
    /// Source name for log filtering (matches LogEntry.source).
    pub source: String,
    /// Status tag text (e.g., "SETUP", "RUN", "DONE", "FAIL").
    pub status_tag: String,
    /// Color for the status tag.
    pub status_color: Color,
    /// Whether this source is currently visible in the log view.
    pub visible: bool,
    /// Whether this is the task entry (vs a process entry).
    pub is_task: bool,
}

/// Build the list of sidebar entries from current task/process state.
///
/// Returns a Vec where index 0 is the task, and subsequent entries are
/// processes ordered: running first (by spawn order), then completed.
pub fn build_sidebar_entries(
    task_name: Option<&str>,
    task_status: &TaskStatus,
    processes: &[ProcessInfo],
    visible_sources: &std::collections::HashSet<String>,
    source_colors: &mut SourceColors,
) -> Vec<SidebarEntry> {
    let mut entries = Vec::new();

    // Task entry
    if let Some(name) = task_name {
        let (tag, color) = task_status_display(task_status);
        // Ensure a color is assigned for the task source
        let _ = source_colors.color_for(name);
        entries.push(SidebarEntry {
            name: name.to_string(),
            source: name.to_string(),
            status_tag: tag,
            status_color: color,
            visible: visible_sources.is_empty() || visible_sources.contains(name),
            is_task: true,
        });
    }

    // Partition processes into running and completed
    let mut running: Vec<&ProcessInfo> = Vec::new();
    let mut completed: Vec<&ProcessInfo> = Vec::new();

    for proc in processes {
        match proc.status {
            ProcessStatus::Running => running.push(proc),
            _ => completed.push(proc),
        }
    }

    // Running processes
    for proc in &running {
        let _ = source_colors.color_for(&proc.task_name);
        entries.push(SidebarEntry {
            name: proc.display_name().to_string(),
            source: proc.task_name.clone(),
            status_tag: "RUN".to_string(),
            status_color: Color::Green,
            visible: visible_sources.is_empty() || visible_sources.contains(&proc.task_name),
            is_task: false,
        });
    }

    // Completed processes
    for proc in &completed {
        let (tag, color) = process_status_display(&proc.status);
        let _ = source_colors.color_for(&proc.task_name);
        entries.push(SidebarEntry {
            name: proc.display_name().to_string(),
            source: proc.task_name.clone(),
            status_tag: tag,
            status_color: color,
            visible: visible_sources.is_empty() || visible_sources.contains(&proc.task_name),
            is_task: false,
        });
    }

    entries
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
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " processes ",
            Style::default()
                .fg(if state.focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if entries.is_empty() {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "  No task running",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(placeholder, inner);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let max_name_width = inner.width.saturating_sub(8) as usize; // leave room for [TAG]

    for (i, entry) in entries.iter().enumerate() {
        let is_selected = state.focused && i == state.selection;

        // Build the line: "  name  [TAG]" or "> name  [TAG]" if selected
        let prefix = if is_selected { "> " } else { "  " };

        // Source color for the name
        let name_color = source_colors.color_for(&entry.source);
        let name_style = if !entry.visible {
            Style::default().fg(Color::DarkGray) // dimmed when filtered out
        } else {
            Style::default().fg(name_color)
        };

        // Truncate name to fit
        let display_name = if entry.name.len() > max_name_width {
            format!("{}~", &entry.name[..max_name_width.saturating_sub(1)])
        } else {
            entry.name.clone()
        };

        // Right-align the status tag
        let tag = format!("[{}]", entry.status_tag);
        let name_and_tag_len = prefix.len() + display_name.len() + 1 + tag.len();
        let padding = if name_and_tag_len < inner.width as usize {
            " ".repeat(inner.width as usize - name_and_tag_len)
        } else {
            " ".to_string()
        };

        let spans = vec![
            Span::styled(
                prefix.to_string(),
                if is_selected {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(display_name, name_style),
            Span::raw(padding),
            Span::styled(tag, Style::default().fg(entry.status_color)),
        ];

        // If this is a section separator, add a blank line before completed processes
        if i > 0 && !entry.is_task {
            // Check if this is the first completed entry (transition from running to non-running)
            let prev = &entries[i - 1];
            let prev_is_running = prev.status_tag == "RUN" || prev.is_task;
            let curr_is_completed = entry.status_tag != "RUN";
            if prev_is_running && curr_is_completed && !entries[i - 1].is_task {
                lines.push(Line::from(""));
            }
        }

        let mut line = Line::from(spans);
        if is_selected {
            line = line.patch_style(Style::default().bg(Color::DarkGray));
        }
        lines.push(line);
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Get display tag and color for a TaskStatus.
fn task_status_display(status: &TaskStatus) -> (String, Color) {
    match status {
        TaskStatus::Setup => ("SETUP".to_string(), Color::Yellow),
        TaskStatus::Ready => ("READY".to_string(), Color::Green),
        TaskStatus::Done => ("DONE".to_string(), Color::DarkGray),
        TaskStatus::Failed(_) => ("FAIL".to_string(), Color::Red)
    }
}

/// Get display tag and color for a ProcessStatus.
fn process_status_display(status: &ProcessStatus) -> (String, Color) {
    match status {
        ProcessStatus::Running => ("RUN".to_string(), Color::Green),
        ProcessStatus::Done => ("DONE".to_string(), Color::DarkGray),
        ProcessStatus::Failed(code) => (format!("FAIL:{}", code), Color::Red),
        ProcessStatus::Stopped => ("STOP".to_string(), Color::Yellow),
    }
}

/// Get the source name for the Nth sidebar entry (0-indexed).
/// Returns None if the index is out of range.
pub fn source_for_index(entries: &[SidebarEntry], index: usize) -> Option<&str> {
    entries.get(index).map(|e| e.source.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sidebar_state_defaults() {
        let state = SidebarState::new();
        assert!(!state.focused);
        assert_eq!(state.selection, 0);
    }

    #[test]
    fn sidebar_state_move_up_clamps() {
        let mut state = SidebarState::new();
        state.selection = 0;
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
        assert_eq!(state.selection, 2); // clamped
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
        let entries = build_sidebar_entries(
            None,
            &TaskStatus::Setup,
            &[],
            &HashSet::new(),
            &mut sc,
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn build_entries_task_only() {
        let mut sc = SourceColors::new();
        let entries = build_sidebar_entries(
            Some("my-task"),
            &TaskStatus::Setup,
            &[],
            &HashSet::new(),
            &mut sc,
        );
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_task);
        assert_eq!(entries[0].name, "my-task");
        assert_eq!(entries[0].status_tag, "SETUP");
    }

    #[test]
    fn task_status_display_all_variants() {
        assert_eq!(task_status_display(&TaskStatus::Setup), ("SETUP".to_string(), Color::Yellow));
        assert_eq!(task_status_display(&TaskStatus::Ready), ("READY".to_string(), Color::Green));
        assert_eq!(task_status_display(&TaskStatus::Done), ("DONE".to_string(), Color::DarkGray));
        let (tag, color) = task_status_display(&TaskStatus::Failed("err".to_string()));
        assert_eq!(tag, "FAIL");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn process_status_display_all_variants() {
        assert_eq!(process_status_display(&ProcessStatus::Running), ("RUN".to_string(), Color::Green));
        assert_eq!(process_status_display(&ProcessStatus::Done), ("DONE".to_string(), Color::DarkGray));
        let (tag, color) = process_status_display(&ProcessStatus::Failed(1));
        assert_eq!(tag, "FAIL:1");
        assert_eq!(color, Color::Red);
        assert_eq!(process_status_display(&ProcessStatus::Stopped), ("STOP".to_string(), Color::Yellow));
    }

    #[test]
    fn source_for_index_valid() {
        let entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: "my-task".to_string(),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: "my-task".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];
        assert_eq!(source_for_index(&entries, 0), Some("my-task"));
        assert_eq!(source_for_index(&entries, 1), Some("my-task"));
        assert_eq!(source_for_index(&entries, 2), None);
    }

    #[test]
    fn build_entries_visibility_with_filter() {
        let mut sc = SourceColors::new();
        let mut visible = HashSet::new();
        visible.insert("visible-source".to_string());

        let entries = build_sidebar_entries(
            Some("my-task"),
            &TaskStatus::Ready,
            &[],
            &visible,
            &mut sc,
        );
        // Task name "my-task" is not in the visible set, so not visible
        assert!(!entries[0].visible);
    }
}
