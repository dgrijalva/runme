use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use tokio::sync::Mutex;

use crate::log::LogEntry;
use crate::log::filter as log_filter;
use crate::log::store::LogStore;
use crate::task::TaskDef;

use super::event::run_event_loop;
use super::filter::{FilterInputState, filter_status_spans, render_filter_input};
use super::render::{DisplayMode, SourceColors};
use super::runner::{ProcessInfo, TaskRunner, TaskStatus};
use super::sidebar::{self, SidebarEntry, SidebarState, SIDEBAR_WIDTH};
use super::viewport::{self, ScrollState, new_entries_since_pin};

/// The mode the application is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Log viewer, navigating with keyboard
    Normal,
    /// Filter expression input mode
    FilterInput,
}

/// Core application state, shared across the event loop and rendering.
pub struct AppState {
    /// Which view/mode is active
    pub mode: AppMode,
    /// Whether the UI needs to be redrawn
    pub dirty: bool,
    /// Whether the main loop should continue running
    pub running: bool,
    /// Log entries currently displayed (tail of the composed log store).
    /// Populated by the event loop from the LogStore broadcast.
    pub log_lines: Vec<LogEntry>,
    /// The LogStore, shared with the runner.
    pub log_store: Arc<Mutex<LogStore>>,
    /// Current task status, if a task is running.
    pub task_status: Option<Arc<Mutex<TaskStatus>>>,
    /// Name of the running task, if any.
    pub task_name: Option<String>,
    /// Display mode: preview (structured) or raw
    pub display_mode: DisplayMode,
    /// Whether to wrap long lines (true) or truncate (false)
    pub wrap: bool,
    /// Scroll state: tail or pinned
    pub scroll: ScrollState,
    /// Source color assignments, consistent within the session
    pub source_colors: SourceColors,
    /// Sidebar state (focus, selection).
    pub sidebar: SidebarState,
    /// Source visibility filter. Empty means show all sources.
    /// When non-empty, only entries whose source is in this set are shown.
    pub source_filter: HashSet<String>,
    /// Cached sidebar entries, rebuilt each frame from process state.
    pub sidebar_entries: Vec<SidebarEntry>,
    /// Process info from the runner, shared for status monitoring.
    pub processes: Option<Arc<Mutex<Vec<ProcessInfo>>>>,
    /// Filter input state for the filter bar.
    pub filter_input: FilterInputState,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            mode: AppMode::Normal,
            dirty: true,
            running: true,
            log_lines: Vec::new(),
            log_store: Arc::new(Mutex::new(LogStore::new())),
            task_status: None,
            task_name: None,
            display_mode: DisplayMode::Preview,
            wrap: false,
            scroll: ScrollState::Tail,
            source_colors: SourceColors::new(),
            sidebar: SidebarState::new(),
            source_filter: HashSet::new(),
            sidebar_entries: Vec::new(),
            processes: None,
            filter_input: FilterInputState::new(),
        }
    }

    /// Get the visible log lines (filtered by source_filter AND expression filter).
    /// Returns indices into self.log_lines for entries that pass both filters.
    pub fn visible_line_indices(&self) -> Vec<usize> {
        let expr = self.filter_input.active_expr();
        self.log_lines
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                // Source filter
                if !self.source_filter.is_empty() && !self.source_filter.contains(&entry.source) {
                    return false;
                }
                // Expression filter
                if let Some(expr) = expr {
                    if !log_filter::matches(expr, entry) {
                        return false;
                    }
                }
                true
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Get the filtered log lines based on source_filter AND expression filter.
    pub fn visible_log_lines(&self) -> Vec<&LogEntry> {
        let expr = self.filter_input.active_expr();
        self.log_lines
            .iter()
            .filter(|entry| {
                // Source filter
                if !self.source_filter.is_empty() && !self.source_filter.contains(&entry.source) {
                    return false;
                }
                // Expression filter
                if let Some(expr) = expr {
                    if !log_filter::matches(expr, entry) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// Toggle visibility of a source. If source_filter is empty (show all),
    /// switching to filtered mode: add all sources except the toggled one.
    /// If already filtered, toggle the specific source.
    pub fn toggle_source_visibility(&mut self, source: &str) {
        if self.source_filter.is_empty() {
            // Currently showing all — switch to "all except this one"
            // Collect all unique sources
            let all_sources: HashSet<String> = self
                .sidebar_entries
                .iter()
                .map(|e| e.source.clone())
                .collect();
            self.source_filter = all_sources;
            self.source_filter.remove(source);
        } else if self.source_filter.contains(source) {
            self.source_filter.remove(source);
            // If filter is now empty after removal, that means nothing is visible
            // which isn't useful. If only this source was visible, keep it.
            // Actually, removing from the visible set means hiding it.
        } else {
            self.source_filter.insert(source.to_string());
            // Check if all sources are now visible — if so, clear the filter
            let all_sources: HashSet<String> = self
                .sidebar_entries
                .iter()
                .map(|e| e.source.clone())
                .collect();
            if self.source_filter == all_sources {
                self.source_filter.clear();
            }
        }
    }

    /// Show all sources (clear the filter).
    pub fn show_all_sources(&mut self) {
        self.source_filter.clear();
    }
}

/// The top-level TUI application. Manages terminal setup/teardown and delegates
/// to the event loop.
pub struct App {
    pub state: AppState,
    /// The task runner, if a task was launched. Stored here to keep it alive;
    /// the runner's state is accessed through the shared Arc fields on AppState.
    #[allow(dead_code)]
    runner: Option<TaskRunner>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            state: AppState::new(),
            runner: None,
        }
    }

    /// Create an App configured to run a specific task immediately.
    pub fn with_task(task: &'static TaskDef) -> Self {
        let mut runner = TaskRunner::new();
        let log_store = runner.log_store.clone();
        let task_status = runner.status.clone();
        let processes = runner.processes.clone();

        runner.launch(task);

        let mut state = AppState::new();
        state.log_store = log_store;
        state.task_status = Some(task_status);
        state.task_name = Some(task.name.to_string());
        state.processes = Some(processes);

        Self {
            state,
            runner: Some(runner),
        }
    }

    /// Enter the TUI: set up the terminal, run the event loop, and restore
    /// the terminal on exit (including panics).
    pub async fn run(&mut self) -> io::Result<()> {
        // Install a panic hook that restores the terminal before unwinding.
        // This is critical — without it, a panic leaves the terminal in raw mode.
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            original_hook(info);
        }));

        setup_terminal()?;

        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;

        let result = run_event_loop(&mut self.state, &mut terminal).await;

        restore_terminal()?;

        // Restore the default panic hook now that the terminal is restored.
        let _ = std::panic::take_hook();

        result
    }
}

/// Render a single frame. Draws the sidebar (left), log viewer (right), and
/// status bar (bottom).
pub fn render_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        // Vertical layout: main content (fills) + status bar (1 line)
        let vert_chunks = Layout::vertical([
            Constraint::Min(0),    // main content area
            Constraint::Length(1), // status bar
        ])
        .split(area);

        let content_area = vert_chunks[0];

        // Horizontal layout: sidebar (fixed width) + log viewer (fills)
        let has_task = state.task_name.is_some();
        let horiz_chunks = if has_task {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(SIDEBAR_WIDTH),
                    Constraint::Min(0),
                ])
                .split(content_area)
        } else {
            // No task running — full-width log viewer
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(0),
                    Constraint::Min(0),
                ])
                .split(content_area)
        };

        let sidebar_area = horiz_chunks[0];
        let log_area = horiz_chunks[1];

        // -- Sidebar --
        if has_task {
            sidebar::render_sidebar(
                frame,
                sidebar_area,
                &state.sidebar_entries,
                &state.sidebar,
                &mut state.source_colors,
            );
        }

        // -- Log viewer --
        let log_width = log_area.width;
        let log_height = log_area.height;

        // Build the filtered log lines for display
        let visible_entries: Vec<&LogEntry> = state.visible_log_lines();

        let lines: Vec<Line> = if visible_entries.is_empty() {
            if state.task_name.is_some() {
                if state.log_lines.is_empty() {
                    vec![Line::from(Span::styled(
                        "  Waiting for output...",
                        Style::default().fg(Color::DarkGray),
                    ))]
                } else if state.filter_input.has_active_filter() {
                    vec![Line::from(Span::styled(
                        "  No entries match the current filter. Press 'f' to edit.",
                        Style::default().fg(Color::DarkGray),
                    ))]
                } else {
                    vec![Line::from(Span::styled(
                        "  All sources filtered out. Press 'a' to show all.",
                        Style::default().fg(Color::DarkGray),
                    ))]
                }
            } else {
                vec![Line::from(Span::styled(
                    "  No task running. Press q to quit.",
                    Style::default().fg(Color::DarkGray),
                ))]
            }
        } else {
            // Convert filtered entries to a contiguous slice for viewport
            let owned_entries: Vec<LogEntry> = visible_entries.into_iter().cloned().collect();

            // Use the viewport to compute which entries are visible
            let vp_layout = viewport::layout(
                &state.scroll,
                &owned_entries,
                log_height,
                log_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );

            // Build a line buffer for the entire viewport, initialized to empty
            let mut line_buffer: Vec<Line<'static>> = (0..log_height)
                .map(|_| Line::from(""))
                .collect();

            // Place rendered entries into the buffer at their Y positions
            let cursor_style = Style::default().bg(Color::DarkGray);
            for ve in &vp_layout.entries {
                for (line_offset, line) in ve.lines.iter().enumerate() {
                    let y = ve.y as usize + line_offset;
                    if y < log_height as usize {
                        if ve.is_cursor {
                            // Highlight the focused row
                            let highlighted = line.clone().patch_style(cursor_style);
                            line_buffer[y] = highlighted;
                        } else {
                            line_buffer[y] = line.clone();
                        }
                    }
                }
            }

            line_buffer
        };

        let log_paragraph = Paragraph::new(lines).block(Block::default());
        frame.render_widget(log_paragraph, log_area);

        // -- Status bar --
        if state.mode == AppMode::FilterInput {
            // Render the filter input bar instead of the normal status bar
            render_filter_input(frame, vert_chunks[1], &state.filter_input);
        } else {
            let mode_text = match state.mode {
                AppMode::Normal => "NORMAL",
                AppMode::FilterInput => "FILTER", // won't reach here, but exhaustive
            };

            let focus_text = if state.sidebar.focused {
                "SIDEBAR"
            } else {
                mode_text
            };

            // Build status line with task info
            let mut spans = vec![
                Span::styled(" runme ", Style::default().fg(Color::Black).bg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(
                    format!(" {} ", focus_text),
                    Style::default().fg(Color::Black).bg(Color::DarkGray),
                ),
            ];

            // Add task name if running
            if let Some(name) = &state.task_name {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!(" {} ", name),
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ));
            }

            // Display mode indicator
            let mode_indicator = match state.display_mode {
                DisplayMode::Preview => "preview",
                DisplayMode::Raw => "raw",
            };
            let wrap_indicator = if state.wrap { "wrap" } else { "truncate" };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!(" {} {} ", mode_indicator, wrap_indicator),
                Style::default().fg(Color::DarkGray),
            ));

            // Source filter indicator
            if !state.source_filter.is_empty() {
                let hidden_count = state
                    .sidebar_entries
                    .iter()
                    .filter(|e| !e.visible)
                    .count();
                if hidden_count > 0 {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        format!(" {} hidden ", hidden_count),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }

            // Active expression filter indicator
            spans.extend(filter_status_spans(&state.filter_input));

            // Scroll position / entry count — use visible count
            let visible_count = state.visible_line_indices().len();
            if visible_count > 0 {
                spans.push(Span::raw(" "));
                match state.scroll {
                    ScrollState::Tail => {
                        spans.push(Span::styled(
                            format!(" TAIL | {} ", visible_count),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    ScrollState::Pinned { cursor, .. } => {
                        let new_count = new_entries_since_pin(&state.scroll, visible_count);
                        let pos_text = if new_count > 0 {
                            format!(" {} / {} (+{} new) ", cursor + 1, visible_count, new_count)
                        } else {
                            format!(" {} / {} ", cursor + 1, visible_count)
                        };
                        spans.push(Span::styled(pos_text, Style::default().fg(Color::DarkGray)));
                    }
                }
            }

            let status_line = Line::from(spans);

            let status_bar = Paragraph::new(status_line)
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));

            frame.render_widget(status_bar, vert_chunks[1]);
        }
    })?;

    Ok(())
}

/// Enter raw mode, switch to the alternate screen, and enable mouse capture.
fn setup_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    Ok(())
}

/// Restore the terminal to its original state.
fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{LogEntry, ParsedContent};
    use std::collections::HashMap;

    fn make_entry(source: &str) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: format!("entry from {}", source),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq: 0,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(format!("entry from {}", source)),
            fields: HashMap::new(),
        }
    }

    #[test]
    fn app_state_defaults() {
        let state = AppState::new();
        assert_eq!(state.mode, AppMode::Normal);
        assert!(state.dirty);
        assert!(state.running);
        assert!(state.log_lines.is_empty());
        assert!(state.task_name.is_none());
        assert!(state.task_status.is_none());
        assert_eq!(state.display_mode, DisplayMode::Preview);
        assert!(!state.wrap);
        assert_eq!(state.scroll, ScrollState::Tail);
        assert!(!state.sidebar.focused);
        assert_eq!(state.sidebar.selection, 0);
        assert!(state.source_filter.is_empty());
        assert!(state.sidebar_entries.is_empty());
        assert!(state.processes.is_none());
    }

    #[test]
    fn app_state_can_be_modified() {
        let mut state = AppState::new();
        state.dirty = false;
        state.running = false;
        state.display_mode = DisplayMode::Raw;
        state.wrap = true;
        assert!(!state.dirty);
        assert!(!state.running);
        assert_eq!(state.display_mode, DisplayMode::Raw);
        assert!(state.wrap);
    }

    #[test]
    fn app_state_scroll_transitions() {
        let mut state = AppState::new();
        assert_eq!(state.scroll, ScrollState::Tail);

        state.scroll = ScrollState::Pinned { cursor: 5, top: 0 };
        assert!(matches!(state.scroll, ScrollState::Pinned { cursor: 5, .. }));

        state.scroll = ScrollState::Tail;
        assert_eq!(state.scroll, ScrollState::Tail);
    }

    #[test]
    fn visible_lines_no_filter() {
        let mut state = AppState::new();
        state.log_lines.push(make_entry("api"));
        state.log_lines.push(make_entry("worker"));
        state.log_lines.push(make_entry("api"));

        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn visible_lines_with_filter() {
        let mut state = AppState::new();
        state.log_lines.push(make_entry("api"));
        state.log_lines.push(make_entry("worker"));
        state.log_lines.push(make_entry("api"));

        state.source_filter.insert("api".to_string());

        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn show_all_clears_filter() {
        let mut state = AppState::new();
        state.source_filter.insert("api".to_string());
        assert!(!state.source_filter.is_empty());
        state.show_all_sources();
        assert!(state.source_filter.is_empty());
    }

    #[test]
    fn toggle_source_from_all_visible() {
        let mut state = AppState::new();
        // Set up sidebar entries so toggle knows about all sources
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: "task".to_string(),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: "api".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];

        // Toggle "api" off — should switch from "all" to "all except api"
        state.toggle_source_visibility("api");
        assert!(state.source_filter.contains("task"));
        assert!(!state.source_filter.contains("api"));
    }

    #[test]
    fn toggle_source_back_on_clears_filter() {
        let mut state = AppState::new();
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: "task".to_string(),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: "api".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];

        // Toggle "api" off then back on
        state.toggle_source_visibility("api");
        assert!(!state.source_filter.is_empty());
        state.toggle_source_visibility("api");
        // All sources are visible again, so filter should be cleared
        assert!(state.source_filter.is_empty());
    }
}
