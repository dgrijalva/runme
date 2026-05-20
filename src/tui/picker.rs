//! Task picker — fuzzy-find task selection UI.
//!
//! Displayed on startup when no task name is provided. Shows all tasks
//! grouped by their `TaskDef.group`, with fuzzy filtering as you type.

use std::collections::HashMap;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
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

/// Which panel of the split picker has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickerFocus {
    TaskList,
    ArgsInput,
}

/// Result of validating the args input against a task's clap command.
#[derive(Clone, Debug)]
pub enum ArgsValidation {
    /// No clap command on the task — we accept anything but can't validate.
    NoMetadata,
    /// shell_words::split or clap parsing failed.
    Error(String),
    /// Input parses cleanly.
    Ok,
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
    /// Which panel has focus.
    pub focus: PickerFocus,
    /// Argument input text (raw, shell-style).
    pub args_input: String,
    /// Cursor byte offset within `args_input`.
    pub args_cursor: usize,
    /// Vertical scroll offset for the help text.
    pub args_help_scroll: u16,
    /// Cached validation result for the current input.
    pub args_validation: ArgsValidation,
    /// Cached rendered help text for the currently-selected task.
    pub cached_help: String,
    /// Identity of the task whose help/args are currently cached. We use the
    /// `&'static TaskDef` pointer so we can detect selection changes cheaply.
    cached_for: Option<*const TaskDef>,
    /// Last drawn screen rect of the right panel — used by mouse hit-testing
    /// to direct scroll events to the help pane.
    pub last_right_panel_rect: Option<Rect>,
    /// Last drawn screen rect of the left panel — for symmetry / future use.
    pub last_left_panel_rect: Option<Rect>,
    /// The fuzzy matcher instance.
    matcher: SkimMatcherV2,
}

// Safety: the `*const TaskDef` is only used for pointer-equality comparison
// to detect selection changes; never dereferenced. Sound to send across.
unsafe impl Send for PickerState {}
unsafe impl Sync for PickerState {}

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

        let mut picker = Self {
            tasks: picker_tasks,
            input: String::new(),
            cursor: 0,
            selection: 0,
            scroll_offset: 0,
            focus: PickerFocus::TaskList,
            args_input: String::new(),
            args_cursor: 0,
            args_help_scroll: 0,
            args_validation: ArgsValidation::Ok,
            cached_help: String::new(),
            cached_for: None,
            last_right_panel_rect: None,
            last_left_panel_rect: None,
            matcher: SkimMatcherV2::default(),
        };
        // Initial selection lands on a group header in browse mode; snap it
        // forward to the first real task so callers don't need to.
        picker.snap_selection_to_first_task();
        picker
    }

    /// Qualified name of the currently-selected task, for use as a key in
    /// the per-session args memory. `None` if no task is selected.
    pub fn selected_qualified_name(&self) -> Option<String> {
        let items = self.visible_items();
        if items.is_empty() {
            return None;
        }
        let idx = self.selection.min(items.len().saturating_sub(1));
        match &items[idx] {
            PickerItem::Task(pt) => Some(pt.qualified_name.clone()),
            PickerItem::GroupHeader(_) => None,
        }
    }

    /// Insert a character into the args input at the cursor.
    pub fn insert_arg_char(&mut self, ch: char) {
        self.args_input.insert(self.args_cursor, ch);
        self.args_cursor += ch.len_utf8();
    }

    /// Delete the character before the args cursor.
    pub fn delete_arg_char(&mut self) {
        if self.args_cursor > 0 {
            let prev = self.args_input[..self.args_cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.args_input.remove(prev);
            self.args_cursor = prev;
        }
    }

    /// Move the args cursor left by one character.
    pub fn arg_cursor_left(&mut self) {
        if self.args_cursor > 0 {
            let prev = self.args_input[..self.args_cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.args_cursor = prev;
        }
    }

    /// Move the args cursor right by one character.
    pub fn arg_cursor_right(&mut self) {
        if self.args_cursor < self.args_input.len() {
            let next = self.args_input[self.args_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.args_cursor + i)
                .unwrap_or(self.args_input.len());
            self.args_cursor = next;
        }
    }

    /// Move the args cursor to the start of the input.
    pub fn arg_cursor_home(&mut self) {
        self.args_cursor = 0;
    }

    /// Move the args cursor to the end of the input.
    pub fn arg_cursor_end(&mut self) {
        self.args_cursor = self.args_input.len();
    }

    /// Scroll the help pane down by `n` lines, capped at the rendered
    /// content height (best-effort — we don't pre-measure).
    pub fn scroll_help_down(&mut self, n: u16) {
        self.args_help_scroll = self.args_help_scroll.saturating_add(n);
    }

    /// Scroll the help pane up by `n` lines.
    pub fn scroll_help_up(&mut self, n: u16) {
        self.args_help_scroll = self.args_help_scroll.saturating_sub(n);
    }

    /// Re-cache the help text and recompute validation for the current
    /// selection. Idempotent if the selection hasn't changed.
    ///
    /// Pulls any stored input from `task_args_memory` (keyed by qualified
    /// name) and resets help scroll on selection change. Should be called
    /// whenever the selection may have changed (after `move_up`/`move_down`,
    /// after typing in the filter input, when opening the picker).
    pub fn refresh_for_selection(&mut self, task_args_memory: &HashMap<String, String>) {
        let task = self.selected_task();
        let new_ptr = task.map(|t| t as *const TaskDef);
        if self.cached_for == new_ptr {
            // Selection unchanged. Re-validate (input may have changed).
            self.args_validation = validate_args(task, &self.args_input);
            return;
        }

        // Selection changed: save would-be input goes through caller; here we
        // just load the new task's stored input.
        self.cached_for = new_ptr;
        self.args_help_scroll = 0;

        if let Some(task) = task {
            self.cached_help = match (task.arg_metadata)() {
                Some(mut cmd) => cmd.render_help().to_string(),
                None => "No arguments.".to_string(),
            };
            let key = self
                .selected_qualified_name()
                .unwrap_or_else(|| task.name.to_string());
            self.args_input = task_args_memory.get(&key).cloned().unwrap_or_default();
            self.args_cursor = self.args_input.len();
        } else {
            self.cached_help.clear();
            self.args_input.clear();
            self.args_cursor = 0;
        }

        self.args_validation = validate_args(task, &self.args_input);
    }

    /// Parse the current args input into argv. Returns the empty vec when
    /// the input is empty or fails to split (we let the launch attempt
    /// surface the error).
    pub fn parsed_argv(&self) -> Vec<String> {
        if self.args_input.trim().is_empty() {
            return Vec::new();
        }
        shell_words::split(&self.args_input).unwrap_or_default()
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

/// Validate `input` against the task's clap command. Empty input is always
/// `Ok`. Tasks without `arg_metadata` return `NoMetadata`.
fn validate_args(task: Option<&'static TaskDef>, input: &str) -> ArgsValidation {
    let Some(task) = task else {
        return ArgsValidation::Ok;
    };
    let Some(cmd) = (task.arg_metadata)() else {
        return ArgsValidation::NoMetadata;
    };
    if input.trim().is_empty() {
        return ArgsValidation::Ok;
    }
    let argv = match shell_words::split(input) {
        Ok(v) => v,
        Err(e) => return ArgsValidation::Error(format!("invalid quoting: {e}")),
    };
    let mut full = Vec::with_capacity(argv.len() + 1);
    full.push(task.name.to_string());
    full.extend(argv);
    match cmd.clone().try_get_matches_from(full) {
        Ok(_) => ArgsValidation::Ok,
        Err(e) => {
            let msg = e
                .to_string()
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("invalid arguments")
                .to_string();
            ArgsValidation::Error(msg)
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
///
/// The picker is split 50/50 horizontally: task list on the left,
/// argument input + help on the right.
pub fn render_picker(frame: &mut ratatui::Frame, area: Rect, picker: &mut PickerState) {
    frame.render_widget(Clear, area);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let left = columns[0];
    let right = columns[1];

    picker.last_left_panel_rect = Some(left);
    picker.last_right_panel_rect = Some(right);

    render_task_list(frame, left, picker);
    render_args_panel(frame, right, picker);
}

/// Render the left half: the task list panel.
fn render_task_list(frame: &mut ratatui::Frame, area: Rect, picker: &mut PickerState) {
    // Ensure selection is within bounds and visible. Reserve 2 rows for
    // the outer block borders and 1 row for the input bar.
    picker.ensure_visible(area.height.saturating_sub(2) as usize);

    let items = picker.visible_items();

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
        // Render visible items with scroll offset.
        // Available height = area.height - 2 (block borders) - 1 (input bar).
        let visible_start = picker.scroll_offset;
        let visible_count = (area.height as usize).saturating_sub(3);

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

    let focused = picker.focus == PickerFocus::TaskList;
    let title_style = if focused {
        Style::default()
            .fg(THEME.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(THEME.dim)
    };
    let border_style = if focused {
        Style::default().fg(THEME.accent)
    } else {
        Style::default().fg(THEME.border)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Pick a task ")
        .title_style(title_style)
        .border_style(border_style);

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the right half: argument input box, validation line, and
/// scrollable help text.
fn render_args_panel(frame: &mut ratatui::Frame, area: Rect, picker: &mut PickerState) {
    let focused = picker.focus == PickerFocus::ArgsInput;
    let title_style = if focused {
        Style::default()
            .fg(THEME.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(THEME.dim)
    };
    let outer_border_style = if focused {
        Style::default().fg(THEME.accent)
    } else {
        Style::default().fg(THEME.border)
    };

    // Outer block — wraps the whole right panel.
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" args ")
        .title_style(title_style)
        .border_style(outer_border_style);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Stack: input box (3 rows), validation line (1 row), help (rest).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    render_args_input(frame, chunks[0], picker);
    render_args_validation(frame, chunks[1], picker);
    render_args_help(frame, chunks[2], picker);
}

fn render_args_input(frame: &mut ratatui::Frame, area: Rect, picker: &PickerState) {
    let (border_color, hint_color) = match &picker.args_validation {
        ArgsValidation::Ok => (THEME.level_info, THEME.dim),
        ArgsValidation::NoMetadata => (THEME.border, THEME.dim),
        ArgsValidation::Error(_) => (THEME.level_error, THEME.dim),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let line: Line<'static> = if picker.args_input.is_empty() {
        Line::from(Span::styled(
            "type args, e.g. --flag value",
            Style::default().fg(hint_color),
        ))
    } else {
        // Draw the cursor as a reverse-video block. Ratatui doesn't manage
        // a real terminal cursor inside a Paragraph, so we splice one in.
        let cursor = picker.args_cursor;
        let before = picker.args_input[..cursor].to_string();
        let (cursor_char, after) = match picker.args_input[cursor..].chars().next() {
            Some(c) => {
                let next = cursor + c.len_utf8();
                (c.to_string(), picker.args_input[next..].to_string())
            }
            None => (" ".to_string(), String::new()),
        };
        Line::from(vec![
            Span::styled(before, Style::default().fg(Color::White)),
            Span::styled(
                cursor_char,
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Span::styled(after, Style::default().fg(Color::White)),
        ])
    };

    let paragraph = Paragraph::new(line).block(block);
    frame.render_widget(paragraph, area);
}

fn render_args_validation(frame: &mut ratatui::Frame, area: Rect, picker: &PickerState) {
    let (text, color) = match &picker.args_validation {
        ArgsValidation::Ok => ("ok".to_string(), THEME.level_info),
        ArgsValidation::NoMetadata => (String::new(), THEME.dim),
        ArgsValidation::Error(msg) => (msg.clone(), THEME.level_error),
    };
    let line = Line::from(Span::styled(text, Style::default().fg(color)));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_args_help(frame: &mut ratatui::Frame, area: Rect, picker: &PickerState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help ")
        .title_style(Style::default().fg(THEME.dim))
        .border_style(Style::default().fg(THEME.border));

    let paragraph = Paragraph::new(picker.cached_help.clone())
        .block(block)
        .style(Style::default().fg(THEME.dim))
        .wrap(Wrap { trim: false })
        .scroll((picker.args_help_scroll, 0));

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

    fn greet_arg_metadata() -> Option<clap::Command> {
        Some(
            clap::Command::new("greet")
                .arg(
                    clap::Arg::new("name")
                        .long("name")
                        .required(true)
                        .num_args(1),
                )
                .arg(
                    clap::Arg::new("count")
                        .long("count")
                        .num_args(1)
                        .default_value("1"),
                ),
        )
    }

    static TEST_TASK_GREET: TaskDef = TaskDef {
        name: "greet",
        description: Some("Greet someone"),
        group: "",
        dir: "",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: greet_arg_metadata,
        ui_hint: None,
    };

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
        dir: "",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_B: TaskDef = TaskDef {
        name: "test",
        description: Some("Run tests"),
        group: "services/auth",
        dir: "",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_C: TaskDef = TaskDef {
        name: "build",
        description: Some("Build the auth service"),
        group: "services/auth",
        dir: "",
        func: TaskFnKind::Static(dummy_task),
        arg_metadata: no_arg_metadata,
        ui_hint: None,
    };

    static TEST_TASK_D: TaskDef = TaskDef {
        name: "dev",
        description: Some("Start dev server"),
        group: "web-app",
        dir: "",
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

    #[test]
    fn validate_args_empty_input_is_ok() {
        assert!(matches!(
            validate_args(Some(&TEST_TASK_GREET), ""),
            ArgsValidation::Ok
        ));
    }

    #[test]
    fn validate_args_missing_required_is_error() {
        let result = validate_args(Some(&TEST_TASK_GREET), "--count 3");
        assert!(matches!(result, ArgsValidation::Error(_)));
    }

    #[test]
    fn validate_args_valid_input_is_ok() {
        assert!(matches!(
            validate_args(Some(&TEST_TASK_GREET), "--name world --count 3"),
            ArgsValidation::Ok
        ));
    }

    #[test]
    fn validate_args_no_metadata() {
        assert!(matches!(
            validate_args(Some(&TEST_TASK_A), "anything"),
            ArgsValidation::NoMetadata
        ));
    }

    #[test]
    fn validate_args_unbalanced_quotes_is_error() {
        let result = validate_args(Some(&TEST_TASK_GREET), "--name 'unclosed");
        assert!(matches!(result, ArgsValidation::Error(_)));
    }

    #[test]
    fn refresh_for_selection_loads_input_from_memory() {
        let tasks: Vec<&'static TaskDef> = vec![&TEST_TASK_GREET];
        let mut group_names = HashMap::new();
        group_names.insert("".to_string(), ".".to_string());
        let mut picker = PickerState::new(&tasks, &group_names);

        let mut memory = HashMap::new();
        memory.insert("greet".to_string(), "--name world".to_string());

        picker.refresh_for_selection(&memory);

        assert_eq!(picker.args_input, "--name world");
        assert!(matches!(picker.args_validation, ArgsValidation::Ok));
        assert!(picker.cached_help.contains("--name"));
    }

    #[test]
    fn refresh_for_selection_resets_help_scroll_on_change() {
        let tasks: Vec<&'static TaskDef> = vec![&TEST_TASK_GREET, &TEST_TASK_A];
        let mut group_names = HashMap::new();
        group_names.insert("".to_string(), ".".to_string());
        let mut picker = PickerState::new(&tasks, &group_names);
        picker.refresh_for_selection(&HashMap::new());
        picker.args_help_scroll = 10;

        // Move to next task
        picker.move_down();
        picker.refresh_for_selection(&HashMap::new());
        assert_eq!(picker.args_help_scroll, 0);
    }

    #[test]
    fn parsed_argv_splits_input() {
        let tasks: Vec<&'static TaskDef> = vec![&TEST_TASK_GREET];
        let mut group_names = HashMap::new();
        group_names.insert("".to_string(), ".".to_string());
        let mut picker = PickerState::new(&tasks, &group_names);
        picker.args_input = "--name world --count 3".to_string();
        picker.args_cursor = picker.args_input.len();
        let argv = picker.parsed_argv();
        assert_eq!(argv, vec!["--name", "world", "--count", "3"]);
    }

    #[test]
    fn parsed_argv_handles_quoted_values() {
        let tasks: Vec<&'static TaskDef> = vec![&TEST_TASK_GREET];
        let mut group_names = HashMap::new();
        group_names.insert("".to_string(), ".".to_string());
        let mut picker = PickerState::new(&tasks, &group_names);
        picker.args_input = "--name 'hello world'".to_string();
        let argv = picker.parsed_argv();
        assert_eq!(argv, vec!["--name", "hello world"]);
    }
}
