//! Task picker — fuzzy-find task selection UI.
//!
//! Displayed on startup when no task name is provided. Shows all tasks
//! grouped by their `TaskDef.group`, with fuzzy filtering as you type.

use std::collections::HashMap;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::theme::THEME;

use crate::task::TaskDef;

/// A task entry in the picker, with precomputed display information.
#[derive(Clone)]
pub struct PickerTask {
    /// The underlying task definition.
    pub task: &'static TaskDef,
    /// Display name for the task's group.
    pub group_display: String,
    /// Fully qualified name for fuzzy matching: "group > name" or just "name" for root.
    pub qualified_name: String,
}

/// State for the task picker UI.
pub struct PickerState {
    /// All available tasks with display info.
    pub tasks: Vec<PickerTask>,
    /// Current fuzzy filter input.
    pub input: String,
    /// Cursor position within input.
    pub cursor: usize,
    /// Currently selected index in the visible list.
    pub selection: usize,
    /// Scroll offset for the visible list (top of viewport).
    pub scroll_offset: usize,
    /// The fuzzy matcher instance.
    matcher: SkimMatcherV2,
}

impl PickerState {
    /// Create a new PickerState from task definitions and group name mappings.
    pub fn new(tasks: &[&'static TaskDef], group_names: &HashMap<String, String>) -> Self {
        let mut picker_tasks: Vec<PickerTask> = tasks
            .iter()
            .map(|&task| {
                let group_display = group_names.get(task.group).cloned().unwrap_or_else(|| {
                    if task.group.is_empty() {
                        ".".to_string()
                    } else {
                        task.group.to_string()
                    }
                });

                let qualified_name = if task.group.is_empty() {
                    task.name.to_string()
                } else {
                    format!("{} > {}", group_display, task.name)
                };

                PickerTask {
                    task,
                    group_display,
                    qualified_name,
                }
            })
            .collect();

        // Sort by group, then by name within group
        picker_tasks.sort_by(|a, b| {
            a.group_display
                .cmp(&b.group_display)
                .then(a.task.name.cmp(b.task.name))
        });

        Self {
            tasks: picker_tasks,
            input: String::new(),
            cursor: 0,
            selection: 0,
            scroll_offset: 0,
            matcher: SkimMatcherV2::default(),
        }
    }

    /// Get the visible items — either the full grouped list (browse mode)
    /// or a fuzzy-filtered ranked list (search mode).
    pub fn visible_items(&self) -> Vec<PickerItem> {
        if self.input.is_empty() {
            self.browse_items()
        } else {
            self.search_items()
        }
    }

    /// Browse mode: tasks grouped by group with headers.
    fn browse_items(&self) -> Vec<PickerItem> {
        let mut items = Vec::new();
        let mut current_group: Option<&str> = None;

        for pt in &self.tasks {
            let group = pt.group_display.as_str();
            if current_group != Some(group) {
                current_group = Some(group);
                items.push(PickerItem::GroupHeader(group.to_string()));
            }
            items.push(PickerItem::Task(pt.clone()));
        }

        items
    }

    /// Search mode: fuzzy match across qualified names and descriptions,
    /// return a flat ranked list.
    fn search_items(&self) -> Vec<PickerItem> {
        let mut scored: Vec<(i64, &PickerTask)> = self
            .tasks
            .iter()
            .filter_map(|pt| {
                // Match against qualified name and description.
                // Name matches get a large bonus so task names win over
                // incidental matches in descriptions.
                let name_score = self
                    .matcher
                    .fuzzy_match(&pt.qualified_name, &self.input)
                    .map(|s| s + 1000);
                let desc_score = pt
                    .task
                    .description
                    .and_then(|d| self.matcher.fuzzy_match(d, &self.input));
                // Take the best score
                let best = match (name_score, desc_score) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                best.map(|score| (score, pt))
            })
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        scored
            .into_iter()
            .map(|(_, pt)| PickerItem::Task(pt.clone()))
            .collect()
    }

    /// Get the selected task, if any.
    pub fn selected_task(&self) -> Option<&'static TaskDef> {
        let items = self.visible_items();
        if items.is_empty() {
            return None;
        }

        let idx = self.selection.min(items.len().saturating_sub(1));
        match &items[idx] {
            PickerItem::Task(pt) => Some(pt.task),
            PickerItem::GroupHeader(_) => None,
        }
    }

    /// Move selection down, skipping group headers.
    pub fn move_down(&mut self) {
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }
        let max = items.len().saturating_sub(1);
        let mut next = self.selection + 1;
        while next <= max {
            if matches!(items[next], PickerItem::Task(_)) {
                self.selection = next;
                return;
            }
            next += 1;
        }
    }

    /// Move selection up, skipping group headers.
    pub fn move_up(&mut self) {
        let items = self.visible_items();
        if items.is_empty() || self.selection == 0 {
            return;
        }
        let mut prev = self.selection.saturating_sub(1);
        loop {
            if matches!(items[prev], PickerItem::Task(_)) {
                self.selection = prev;
                return;
            }
            if prev == 0 {
                break;
            }
            prev -= 1;
        }
    }

    /// Insert a character into the fuzzy input.
    pub fn insert_char(&mut self, ch: char) {
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        // Reset selection to first task when input changes
        self.selection = 0;
        self.scroll_offset = 0;
        self.snap_selection_to_first_task();
    }

    /// Delete the character before the cursor.
    pub fn delete_char(&mut self) {
        if self.cursor > 0 {
            let prev_char_boundary = self.input[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.remove(prev_char_boundary);
            self.cursor = prev_char_boundary;
            self.selection = 0;
            self.scroll_offset = 0;
            self.snap_selection_to_first_task();
        }
    }

    /// Ensure selection points to a task (not a group header).
    fn snap_selection_to_first_task(&mut self) {
        let items = self.visible_items();
        if items.is_empty() {
            return;
        }
        if self.selection >= items.len() {
            self.selection = 0;
        }
        if matches!(items[self.selection], PickerItem::GroupHeader(_)) {
            // Find next task
            for (i, item) in items.iter().enumerate().skip(self.selection) {
                if matches!(item, PickerItem::Task(_)) {
                    self.selection = i;
                    return;
                }
            }
            // No task after selection; try from beginning
            for (i, item) in items.iter().enumerate() {
                if matches!(item, PickerItem::Task(_)) {
                    self.selection = i;
                    return;
                }
            }
        }
    }

    /// Ensure scroll_offset keeps selection visible within the viewport.
    pub fn ensure_visible(&mut self, viewport_height: usize) {
        // Leave 1 line for the input bar at the top
        let visible_rows = viewport_height.saturating_sub(1);
        if visible_rows == 0 {
            return;
        }
        if self.selection < self.scroll_offset {
            self.scroll_offset = self.selection;
        }
        if self.selection >= self.scroll_offset + visible_rows {
            self.scroll_offset = self.selection.saturating_sub(visible_rows - 1);
        }
    }
}

/// An item in the picker list — either a group header or a task.
#[derive(Clone)]
pub enum PickerItem {
    GroupHeader(String),
    Task(PickerTask),
}

/// Render the task picker into the given area. Used by the frame as
/// a centered overlay (decisions 1 + 8); the caller chooses the size
/// and position.
pub fn render_picker(frame: &mut ratatui::Frame, area: Rect, picker: &mut PickerState) {
    // Ensure selection is within bounds and visible
    picker.ensure_visible(area.height as usize);

    let items = picker.visible_items();

    // Clear the area
    frame.render_widget(Clear, area);

    // Build the lines for the list
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Input bar at the top
    let input_line = if picker.input.is_empty() {
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(THEME.accent)),
            Span::styled("type to filter...", Style::default().fg(THEME.dim)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" > ", Style::default().fg(THEME.accent)),
            Span::styled(picker.input.clone(), Style::default().fg(Color::White)),
        ])
    };
    lines.push(input_line);

    if items.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  No tasks match the filter.",
            Style::default().fg(THEME.dim),
        )));
    } else {
        // Render visible items with scroll offset
        let visible_start = picker.scroll_offset;
        let visible_count = (area.height as usize).saturating_sub(1); // -1 for input bar

        for (idx, item) in items
            .iter()
            .enumerate()
            .skip(visible_start)
            .take(visible_count)
        {
            let line = match item {
                PickerItem::GroupHeader(name) => {
                    let display = if name.is_empty() { "." } else { name.as_str() };
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            display.to_string(),
                            Style::default()
                                .fg(THEME.level_warn)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                }
                PickerItem::Task(pt) => {
                    let is_selected = idx == picker.selection;
                    let indicator = if is_selected { "> " } else { "    " };
                    let name_style = if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let desc_style = if is_selected {
                        Style::default().fg(THEME.accent)
                    } else {
                        Style::default().fg(THEME.dim)
                    };
                    let bg_style = if is_selected {
                        Style::default().bg(THEME.selection_bg)
                    } else {
                        Style::default()
                    };

                    let desc = pt
                        .task
                        .description
                        .map(|d| format!("  {}", d))
                        .unwrap_or_default();

                    Line::from(vec![
                        Span::styled(indicator.to_string(), bg_style.fg(THEME.accent)),
                        Span::styled(pt.task.name.to_string(), name_style.patch(bg_style)),
                        Span::styled(desc, desc_style.patch(bg_style)),
                    ])
                }
            };
            lines.push(line);
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Pick a task ")
        .title_style(
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(THEME.border));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TaskError;
    use crate::task::{TaskContext, TaskFnKind};
    use std::future::Future;
    use std::pin::Pin;

    fn no_arg_metadata() -> Option<clap::Command> {
        None
    }

    fn dummy_task<'a>(
        _ctx: &'a TaskContext,
        _args: &[String],
    ) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    static TEST_TASK_A: TaskDef = TaskDef {
        name: "build",
        description: Some("Build the project"),
        group: "",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_B: TaskDef = TaskDef {
        name: "test",
        description: Some("Run tests"),
        group: "services/auth",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_C: TaskDef = TaskDef {
        name: "build",
        description: Some("Build the auth service"),
        group: "services/auth",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_D: TaskDef = TaskDef {
        name: "dev",
        description: Some("Start dev server"),
        group: "web-app",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    fn make_picker() -> PickerState {
        let tasks: Vec<&'static TaskDef> =
            vec![&TEST_TASK_A, &TEST_TASK_B, &TEST_TASK_C, &TEST_TASK_D];
        let mut group_names = HashMap::new();
        group_names.insert("".to_string(), ".".to_string());
        group_names.insert("services/auth".to_string(), "services/auth".to_string());
        group_names.insert("web-app".to_string(), "web-app".to_string());
        PickerState::new(&tasks, &group_names)
    }

    #[test]
    fn test_browse_mode_groups_tasks() {
        let picker = make_picker();
        let items = picker.visible_items();

        // Should have group headers + tasks
        let headers: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                PickerItem::GroupHeader(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();

        assert!(headers.contains(&"."));
        assert!(headers.contains(&"services/auth"));
        assert!(headers.contains(&"web-app"));
    }

    #[test]
    fn test_browse_mode_task_count() {
        let picker = make_picker();
        let items = picker.visible_items();

        let task_count = items
            .iter()
            .filter(|i| matches!(i, PickerItem::Task(_)))
            .count();

        assert_eq!(task_count, 4);
    }

    #[test]
    fn test_fuzzy_filter_narrows_results() {
        let mut picker = make_picker();
        picker.insert_char('d');
        picker.insert_char('e');
        picker.insert_char('v');

        let items = picker.visible_items();
        let task_count = items
            .iter()
            .filter(|i| matches!(i, PickerItem::Task(_)))
            .count();

        // "dev" should match at least the "dev" task
        assert!(task_count >= 1);

        // The top result should be the dev task
        if let Some(PickerItem::Task(pt)) = items.first() {
            assert_eq!(pt.task.name, "dev");
        }
    }

    #[test]
    fn test_fuzzy_filter_matches_description() {
        let mut picker = make_picker();
        // Type "server" which appears in the dev task's description
        for ch in "server".chars() {
            picker.insert_char(ch);
        }

        let items = picker.visible_items();
        let tasks: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                PickerItem::Task(pt) => Some(pt.task.name),
                _ => None,
            })
            .collect();

        assert!(
            tasks.contains(&"dev"),
            "expected 'dev' in results: {:?}",
            tasks
        );
    }

    #[test]
    fn test_fuzzy_filter_matches_qualified_name() {
        let mut picker = make_picker();
        // Type "auth" which is in the group name
        for ch in "auth".chars() {
            picker.insert_char(ch);
        }

        let items = picker.visible_items();
        let tasks: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                PickerItem::Task(pt) => Some(pt.task.name),
                _ => None,
            })
            .collect();

        // Should include auth group tasks
        assert!(
            tasks.contains(&"test") || tasks.contains(&"build"),
            "expected auth group tasks in results: {:?}",
            tasks
        );
    }

    #[test]
    fn test_navigation_skips_headers() {
        let mut picker = make_picker();

        // Initially selection should be on the first task (not header)
        picker.snap_selection_to_first_task();
        let items = picker.visible_items();
        assert!(matches!(items[picker.selection], PickerItem::Task(_)));

        // Move down should skip to next task
        let initial = picker.selection;
        picker.move_down();
        assert!(picker.selection > initial);
        let items = picker.visible_items();
        assert!(matches!(items[picker.selection], PickerItem::Task(_)));
    }

    #[test]
    fn test_selected_task_returns_correct_def() {
        let mut picker = make_picker();
        picker.snap_selection_to_first_task();

        let selected = picker.selected_task();
        assert!(selected.is_some());
    }

    #[test]
    fn test_delete_char_reverts_filter() {
        let mut picker = make_picker();
        picker.insert_char('d');
        picker.insert_char('e');
        picker.insert_char('v');

        let filtered_count = picker.visible_items().len();

        // Delete all chars
        picker.delete_char();
        picker.delete_char();
        picker.delete_char();

        let full_count = picker.visible_items().len();
        assert!(full_count > filtered_count);
    }

    #[test]
    fn test_empty_filter_shows_all() {
        let picker = make_picker();
        let items = picker.visible_items();

        // Should have all 4 tasks plus group headers
        let task_count = items
            .iter()
            .filter(|i| matches!(i, PickerItem::Task(_)))
            .count();
        assert_eq!(task_count, 4);
    }
}
