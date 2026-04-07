use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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
use super::output::TuiOutput;
use super::picker::{self, PickerState};
use super::render::{DisplayMode, SourceColors};
use super::runner::{ProcessInfo, TaskRunner, TaskStatus};
use super::search::{SearchState, render_search_input, search_status_spans};
use super::sidebar::{self, SidebarEntry, SidebarState, SIDEBAR_WIDTH};
use super::viewport::{self, ScrollState, new_entries_since_pin};

/// The mode the application is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Task picker — fuzzy-find task selection on startup
    TaskPicker,
    /// Log viewer, navigating with keyboard
    Normal,
    /// Filter expression input mode
    FilterInput,
    /// Search pattern input mode
    SearchInput,
    /// Help overlay
    Help,
    /// Entry detail overlay (expanded log entry view)
    EntryDetail,
    /// Process detail overlay (expanded process info view)
    ProcessDetail,
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
    /// Search state for / search with n/N navigation.
    pub search: SearchState,
    /// Scroll offset within the entry detail overlay (for long entries).
    pub detail_scroll: usize,
    /// Task picker state (only populated when in TaskPicker mode).
    pub picker: Option<PickerState>,
    /// All available tasks for the picker (kept for re-launching).
    pub all_tasks: Vec<&'static TaskDef>,
    /// Group name mapping for display (group_key -> display_name).
    pub group_names: HashMap<String, String>,
    /// Task selected from the picker, pending launch by the event loop.
    pub pending_task: Option<&'static TaskDef>,
    /// The task runner, stored here so the event loop can manage task launches
    /// from the picker without needing access to the App wrapper.
    #[allow(dead_code)]
    pub runner: Option<TaskRunner>,
    /// Index of the process being viewed in the ProcessDetail overlay.
    /// This is the sidebar entry index (not the process vec index).
    pub process_detail_index: Option<usize>,
    /// Scroll offset within the process detail overlay.
    pub process_detail_scroll: usize,
    /// Cached lsof output for the process detail panel.
    pub process_detail_sockets: Option<String>,
    /// Whether the sidebar is visible (can be collapsed with backslash).
    pub sidebar_visible: bool,
    /// Notification messages (text, timestamp for auto-dismiss).
    pub notifications: Vec<(String, std::time::Instant)>,
    /// Filter expression history (session-scoped).
    pub filter_history: Vec<String>,
    /// Current position in filter history (for Up/Down cycling). None = not browsing history.
    pub filter_history_index: Option<usize>,
    /// Whether the TUI should stay open after the task completes.
    /// None when no task is running. Shared with the TaskContext via the runner.
    pub tui_wait: Option<Arc<AtomicBool>>,
    /// Post-TUI output buffer. Shared with the TaskContext via the runner.
    /// Flushed to real stdio after `restore_terminal()`.
    pub tui_output: Option<Arc<Mutex<TuiOutput>>>,
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
            search: SearchState::new(),
            detail_scroll: 0,
            picker: None,
            all_tasks: Vec::new(),
            group_names: HashMap::new(),
            pending_task: None,
            runner: None,
            process_detail_index: None,
            process_detail_scroll: 0,
            process_detail_sockets: None,
            sidebar_visible: true,
            notifications: Vec::new(),
            filter_history: Vec::new(),
            filter_history_index: None,
            tui_wait: None,
            tui_output: None,
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

    /// Launch a task from the picker. Sets up the TaskRunner and transitions
    /// to Normal mode. Called from the event loop when pending_task is set.
    pub fn launch_picked_task(&mut self, task: &'static TaskDef) {
        let mut runner = TaskRunner::new();
        let log_store = runner.log_store.clone();
        let task_status = runner.status.clone();
        let processes = runner.processes.clone();
        let tui_wait = runner.tui_wait.clone();
        let tui_output = runner.tui_output.clone();

        runner.launch(task);

        self.log_store = log_store;
        self.task_status = Some(task_status);
        self.task_name = Some(task.name.to_string());
        self.processes = Some(processes);
        self.tui_wait = Some(tui_wait);
        self.tui_output = Some(tui_output);
        self.mode = AppMode::Normal;
        self.picker = None;
        self.pending_task = None;
        self.log_lines.clear();
        self.dirty = true;
        self.runner = Some(runner);
    }
}

/// The top-level TUI application. Manages terminal setup/teardown and delegates
/// to the event loop.
pub struct App {
    pub state: AppState,
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
        }
    }

    /// Create an App that starts with the task picker.
    ///
    /// Shows all available tasks grouped by their source file, with fuzzy
    /// filtering. The user selects a task to launch.
    pub fn with_picker(
        tasks: Vec<&'static TaskDef>,
        group_names: HashMap<String, String>,
    ) -> Self {
        let picker = PickerState::new(&tasks, &group_names);
        let mut state = AppState::new();
        state.mode = AppMode::TaskPicker;
        state.picker = Some(picker);
        state.all_tasks = tasks;
        state.group_names = group_names;

        Self { state }
    }

    /// Create an App configured to run a specific task immediately.
    pub fn with_task(task: &'static TaskDef) -> Self {
        let mut state = AppState::new();
        state.launch_picked_task(task);
        Self { state }
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

        // Flush staged TUI output to real stdout/stderr now that we've
        // exited the alternate screen.
        if let Some(ref tui_output) = self.state.tui_output {
            let (stdout_text, stderr_text) = tui_output.lock().await.flush().await;
            if !stdout_text.is_empty() {
                use std::io::Write;
                let _ = io::stdout().write_all(stdout_text.as_bytes());
                let _ = io::stdout().flush();
            }
            if !stderr_text.is_empty() {
                use std::io::Write;
                let _ = io::stderr().write_all(stderr_text.as_bytes());
                let _ = io::stderr().flush();
            }
        }

        // Restore the default panic hook now that the terminal is restored.
        let _ = std::panic::take_hook();

        result
    }
}

/// Render a single frame. Draws the sidebar (left), log viewer (right), and
/// status bar (bottom). In TaskPicker mode, renders the picker full-screen instead.
pub fn render_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
) -> io::Result<()> {
    // Task picker mode gets its own full-screen rendering path
    if state.mode == AppMode::TaskPicker {
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some(ref mut picker_state) = state.picker {
                picker::render_picker(frame, area, picker_state);
            }
        })?;
        return Ok(());
    }

    terminal.draw(|frame| {
        let area = frame.area();

        // Vertical layout: main content + optional input bar + status bar
        let has_input_bar = matches!(state.mode, AppMode::FilterInput | AppMode::SearchInput);
        let vert_chunks = if has_input_bar {
            Layout::vertical([
                Constraint::Min(0),    // main content area
                Constraint::Length(1), // input bar
                Constraint::Length(1), // status bar
            ])
            .split(area)
        } else {
            Layout::vertical([
                Constraint::Min(0),    // main content area
                Constraint::Length(0), // no input bar
                Constraint::Length(1), // status bar
            ])
            .split(area)
        };

        let content_area = vert_chunks[0];
        let input_bar_area = vert_chunks[1];
        let status_bar_area = vert_chunks[2];

        // Horizontal layout: sidebar (fixed width) + log viewer (fills)
        let has_task = state.task_name.is_some();
        let show_sidebar = has_task && state.sidebar_visible;
        let horiz_chunks = if show_sidebar {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(SIDEBAR_WIDTH),
                    Constraint::Min(0),
                ])
                .split(content_area)
        } else {
            // No task running or sidebar collapsed — full-width log viewer
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
        if show_sidebar {
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

            // Determine if search highlighting is needed
            let search_pattern = if state.search.active {
                Some(state.search.pattern.clone())
            } else {
                None
            };
            let current_search_entry = state.search.current_match_index();

            // Place rendered entries into the buffer at their Y positions
            let cursor_style = Style::default().bg(Color::DarkGray);
            for ve in &vp_layout.entries {
                // Determine if this entry is the current search match
                let is_current_search_match = current_search_entry == Some(ve.entry_index);

                for (line_offset, line) in ve.lines.iter().enumerate() {
                    let y = ve.y as usize + line_offset;
                    if y < log_height as usize {
                        // Apply search highlighting if search is active
                        let display_line = if let Some(ref pattern) = search_pattern {
                            apply_search_highlight(line, pattern, is_current_search_match)
                        } else {
                            line.clone()
                        };

                        if ve.is_cursor {
                            // Highlight the focused row
                            let highlighted = display_line.patch_style(cursor_style);
                            line_buffer[y] = highlighted;
                        } else {
                            line_buffer[y] = display_line;
                        }
                    }
                }
            }

            line_buffer
        };

        let log_paragraph = Paragraph::new(lines).block(Block::default());
        frame.render_widget(log_paragraph, log_area);

        // -- Input bar (above status bar, only when filter/search input is active) --
        if state.mode == AppMode::FilterInput {
            render_filter_input(frame, input_bar_area, &state.filter_input);
        } else if state.mode == AppMode::SearchInput {
            render_search_input(frame, input_bar_area, &state.search);
        }

        // -- Status bar (always visible) --
        {
            let mode_text = match state.mode {
                AppMode::TaskPicker => "PICKER",
                AppMode::Normal | AppMode::Help => "NORMAL",
                AppMode::FilterInput => "FILTER",
                AppMode::SearchInput => "SEARCH",
                AppMode::EntryDetail => "DETAIL",
                AppMode::ProcessDetail => "PROCESS",
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

            // Active search indicator
            spans.extend(search_status_spans(&state.search));

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

            frame.render_widget(status_bar, status_bar_area);
        }

        // -- Help overlay --
        if state.mode == AppMode::Help {
            render_help_overlay(frame, area);
        }

        // -- Entry detail overlay --
        if state.mode == AppMode::EntryDetail {
            render_entry_detail(frame, area, state);
        }

        // -- Process detail overlay --
        if state.mode == AppMode::ProcessDetail {
            render_process_detail(frame, area, state);
        }

        // -- Notifications (top of log area, auto-dismiss) --
        if !state.notifications.is_empty() {
            render_notifications(frame, log_area, &state.notifications);
        }
    })?;

    Ok(())
}

/// Render a centered help overlay showing keyboard shortcuts.
fn render_help_overlay(frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
    use ratatui::widgets::{Borders, Clear, Wrap};

    let help_text = vec![
        Line::from(Span::styled("Keyboard Shortcuts", Style::default().fg(Color::Cyan).add_modifier(ratatui::style::Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![Span::styled("Navigation", Style::default().fg(Color::Yellow))]),
        Line::from(vec![Span::raw("  j/k    "), Span::styled("Move cursor down/up", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  [/]    "), Span::styled("Page up/down", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  g/G    "), Span::styled("Jump to top / bottom (tail)", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  Enter  "), Span::styled("Open entry detail view", Style::default().fg(Color::DarkGray))]),
        Line::from(""),
        Line::from(vec![Span::styled("Display", Style::default().fg(Color::Yellow))]),
        Line::from(vec![Span::raw("  v      "), Span::styled("Toggle preview/raw mode", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  w      "), Span::styled("Toggle wrap/truncate", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  \\      "), Span::styled("Toggle sidebar visibility", Style::default().fg(Color::DarkGray))]),
        Line::from(""),
        Line::from(vec![Span::styled("Filter & Search", Style::default().fg(Color::Yellow))]),
        Line::from(vec![Span::raw("  f      "), Span::styled("Open filter bar (Enter confirm, Esc cancel)", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  /      "), Span::styled("Open search (Enter confirm, Esc cancel)", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  n/N    "), Span::styled("Next/previous search match", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  Up/Dn  "), Span::styled("Cycle filter history (in filter input)", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  1-9    "), Span::styled("Toggle source N visibility", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  a      "), Span::styled("Show all sources", Style::default().fg(Color::DarkGray))]),
        Line::from(""),
        Line::from(vec![Span::styled("Sidebar (Tab to focus)", Style::default().fg(Color::Yellow))]),
        Line::from(vec![Span::raw("  Tab    "), Span::styled("Toggle sidebar focus", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  Enter  "), Span::styled("Process detail / toggle source visibility", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  s      "), Span::styled("Stop selected process (SIGTERM)", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  S      "), Span::styled("Send SIGHUP to selected process", Style::default().fg(Color::DarkGray))]),
        Line::from(""),
        Line::from(vec![Span::styled("Export", Style::default().fg(Color::Yellow))]),
        Line::from(vec![Span::raw("  e      "), Span::styled("Export visible log to file", Style::default().fg(Color::DarkGray))]),
        Line::from(""),
        Line::from(vec![Span::raw("  q      "), Span::styled("Quit", Style::default().fg(Color::DarkGray))]),
        Line::from(vec![Span::raw("  ?      "), Span::styled("Toggle this help", Style::default().fg(Color::DarkGray))]),
    ];

    let help_height = (help_text.len() + 2) as u16; // +2 for border
    let help_width = 56u16;

    // Center the popup
    let x = area.width.saturating_sub(help_width) / 2;
    let y = area.height.saturating_sub(help_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        help_width.min(area.width),
        help_height.min(area.height),
    );

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    let help_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(" Help ", Style::default().fg(Color::Cyan)));

    let help_paragraph = Paragraph::new(help_text)
        .block(help_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(help_paragraph, popup_area);
}

/// Render the entry detail overlay showing all fields of the focused log entry.
fn render_entry_detail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
) {
    use ratatui::widgets::{Borders, Clear, Wrap};

    // Find the focused entry from the cursor position
    let visible_indices = state.visible_line_indices();
    let cursor_idx = match state.scroll {
        ScrollState::Tail => {
            if visible_indices.is_empty() {
                return;
            }
            *visible_indices.last().unwrap()
        }
        ScrollState::Pinned { cursor, .. } => {
            if cursor >= visible_indices.len() {
                if visible_indices.is_empty() {
                    return;
                }
                *visible_indices.last().unwrap()
            } else {
                visible_indices[cursor]
            }
        }
    };

    let entry = match state.log_lines.get(cursor_idx) {
        Some(e) => e,
        None => return,
    };

    // Build the detail lines
    let mut detail_lines: Vec<Line<'static>> = Vec::new();

    // Well-known fields
    detail_lines.push(Line::from(vec![
        Span::styled("timestamp: ", Style::default().fg(Color::Cyan)),
        Span::raw(entry.display_timestamp()),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("level:     ", Style::default().fg(Color::Cyan)),
        Span::raw(entry.level.clone().unwrap_or_else(|| "---".to_string())),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("source:    ", Style::default().fg(Color::Cyan)),
        Span::raw(entry.source.clone()),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("message:   ", Style::default().fg(Color::Cyan)),
        Span::raw(
            entry
                .message
                .clone()
                .unwrap_or_else(|| "(none)".to_string()),
        ),
    ]));

    // Additional fields
    if !entry.fields.is_empty() {
        detail_lines.push(Line::from(""));

        // Sort fields by key for consistent display
        let mut field_keys: Vec<&String> = entry.fields.keys().collect();
        field_keys.sort();

        // Find the longest key for alignment
        let max_key_len = field_keys.iter().map(|k| k.len()).max().unwrap_or(0);

        for key in field_keys {
            let value = &entry.fields[key];
            let value_str = match value {
                serde_json::Value::String(s) => format!("\"{}\"", s),
                other => other.to_string(),
            };
            let padding = " ".repeat(max_key_len.saturating_sub(key.len()));
            detail_lines.push(Line::from(vec![
                Span::styled(
                    format!("{}:{} ", key, padding),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(value_str),
            ]));
        }
    }

    // Raw text section
    detail_lines.push(Line::from(""));
    detail_lines.push(Line::from(Span::styled(
        "--- raw ---",
        Style::default().fg(Color::DarkGray),
    )));
    for raw_line in entry.raw.lines() {
        detail_lines.push(Line::from(raw_line.to_string()));
    }

    // Compute overlay dimensions — use most of the screen height so
    // wrapped content (like raw JSON) has room to display
    let total_lines = detail_lines.len();
    let max_height = (area.height as usize).saturating_sub(4);
    let display_height = max_height.max(6);
    let display_width = (area.width as usize).saturating_sub(8).max(20);

    let popup_height = (display_height + 2) as u16; // +2 for border
    let popup_width = display_width as u16;

    // Center the popup
    let x = area.width.saturating_sub(popup_width) / 2;
    let y = area.height.saturating_sub(popup_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        popup_width.min(area.width),
        popup_height.min(area.height),
    );

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    // Apply scroll offset — allow scrolling through all content lines
    let scroll_offset = if total_lines > display_height {
        state.detail_scroll.min(total_lines.saturating_sub(1))
    } else {
        0
    };
    let visible_lines: Vec<Line<'static>> = detail_lines
        .into_iter()
        .skip(scroll_offset)
        .collect();

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Entry Detail (j/k scroll, y copy, Esc close) ",
            Style::default().fg(Color::Cyan),
        ));

    let detail_paragraph = Paragraph::new(visible_lines)
        .block(detail_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(detail_paragraph, popup_area);
}

/// Render the process detail overlay showing info about a specific spawned process.
fn render_process_detail(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
) {
    use ratatui::widgets::{Borders, Clear, Wrap};

    let sidebar_idx = match state.process_detail_index {
        Some(idx) => idx,
        None => return,
    };

    let entry = match state.sidebar_entries.get(sidebar_idx) {
        Some(e) => e,
        None => return,
    };

    // Build the detail lines
    let mut detail_lines: Vec<Line<'static>> = Vec::new();

    detail_lines.push(Line::from(vec![
        Span::styled("Command:  ", Style::default().fg(Color::Cyan)),
        Span::raw(entry.name.clone()),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("Source:   ", Style::default().fg(Color::Cyan)),
        Span::raw(entry.source.clone()),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("Status:   ", Style::default().fg(Color::Cyan)),
        Span::styled(
            entry.status_tag.clone(),
            Style::default().fg(entry.status_color),
        ),
    ]));

    // Try to get PID/PGID from the actual process info
    if let Some(procs_arc) = &state.processes {
        if let Ok(procs) = procs_arc.try_lock() {
            // Map sidebar index to process vec index
            // Sidebar index 0 = task, so process offset = sidebar_idx - 1
            // Then map through running/completed ordering
            let proc_idx = if state.task_name.is_some() {
                sidebar_idx.checked_sub(1)
            } else {
                Some(sidebar_idx)
            };

            if let Some(idx) = proc_idx {
                let mut running_indices: Vec<usize> = Vec::new();
                let mut completed_indices: Vec<usize> = Vec::new();
                for (i, p) in procs.iter().enumerate() {
                    if p.status == super::runner::ProcessStatus::Running {
                        running_indices.push(i);
                    } else {
                        completed_indices.push(i);
                    }
                }
                let ordered: Vec<usize> =
                    running_indices.into_iter().chain(completed_indices).collect();

                if let Some(&proc_vec_idx) = ordered.get(idx) {
                    let proc = &procs[proc_vec_idx];

                    if let Some(pid) = proc.pid {
                        detail_lines.push(Line::from(vec![
                            Span::styled("PID:      ", Style::default().fg(Color::Cyan)),
                            Span::raw(pid.to_string()),
                        ]));
                    }

                    if let Some(pgid) = proc.pgid {
                        detail_lines.push(Line::from(vec![
                            Span::styled("PGID:     ", Style::default().fg(Color::Cyan)),
                            Span::raw(pgid.to_string()),
                        ]));
                    }
                }
            }
        }
    }

    // Listening ports
    detail_lines.push(Line::from(""));
    if let Some(ref sockets) = state.process_detail_sockets {
        detail_lines.push(Line::from(vec![
            Span::styled("Ports:   ", Style::default().fg(Color::Cyan)),
            Span::raw(sockets.clone()),
        ]));
    } else {
        detail_lines.push(Line::from(vec![
            Span::styled("Ports:   ", Style::default().fg(Color::Cyan)),
            Span::styled("scanning...", Style::default().fg(Color::Yellow)),
        ]));
    }

    // Controls hint at bottom
    detail_lines.push(Line::from(""));
    detail_lines.push(Line::from(vec![
        Span::styled("s", Style::default().fg(Color::Cyan)),
        Span::raw(" stop (SIGTERM)  "),
        Span::styled("S", Style::default().fg(Color::Cyan)),
        Span::raw(" SIGHUP  "),
        Span::styled("j/k", Style::default().fg(Color::Cyan)),
        Span::raw(" scroll  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" close"),
    ]));

    // Compute overlay dimensions
    let total_lines = detail_lines.len();
    let max_height = (area.height as usize).saturating_sub(4);
    let display_height = max_height.max(6);
    let display_width = (area.width as usize).saturating_sub(8).max(20);

    let popup_height = (display_height + 2) as u16; // +2 for border
    let popup_width = display_width as u16;

    // Center the popup
    let x = area.width.saturating_sub(popup_width) / 2;
    let y = area.height.saturating_sub(popup_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        popup_width.min(area.width),
        popup_height.min(area.height),
    );

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    // Apply scroll offset
    let scroll_offset = if total_lines > display_height {
        state.process_detail_scroll.min(total_lines.saturating_sub(1))
    } else {
        0
    };
    let visible_lines: Vec<Line<'static>> = detail_lines.into_iter().skip(scroll_offset).collect();

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Process Detail ",
            Style::default().fg(Color::Cyan),
        ));

    let detail_paragraph = Paragraph::new(visible_lines)
        .block(detail_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(detail_paragraph, popup_area);
}

/// Render notification banners at the top of the log area.
fn render_notifications(
    frame: &mut ratatui::Frame,
    log_area: ratatui::layout::Rect,
    notifications: &[(String, std::time::Instant)],
) {
    use ratatui::widgets::Clear;

    if notifications.is_empty() || log_area.height < 2 {
        return;
    }

    // Show the most recent notification (at most 1 line)
    let (text, _) = &notifications[notifications.len() - 1];

    let notif_area = ratatui::layout::Rect::new(
        log_area.x,
        log_area.y,
        log_area.width,
        1,
    );

    frame.render_widget(Clear, notif_area);
    let line = Line::from(vec![
        Span::styled(" ! ", Style::default().fg(Color::Black).bg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(text.clone(), Style::default().fg(Color::Yellow)),
    ]);
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, notif_area);
}

/// Apply search highlighting to a rendered line.
///
/// Walks each span in the line, finds substring matches of `pattern` (case-insensitive),
/// and splits the span into highlighted/unhighlighted pieces.
fn apply_search_highlight(
    line: &Line<'static>,
    pattern: &str,
    is_current_match: bool,
) -> Line<'static> {
    use super::search::{find_match_ranges, current_match_highlight_style, match_highlight_style};

    let hl_style = if is_current_match {
        current_match_highlight_style()
    } else {
        match_highlight_style()
    };

    let mut new_spans: Vec<Span<'static>> = Vec::new();
    for span in &line.spans {
        let text: &str = &span.content;
        let ranges = find_match_ranges(text, pattern);
        if ranges.is_empty() {
            new_spans.push(span.clone());
        } else {
            let mut pos = 0;
            for range in &ranges {
                if range.start > pos {
                    new_spans.push(Span::styled(
                        text[pos..range.start].to_string(),
                        span.style,
                    ));
                }
                // Overlay the highlight style on top of the existing span style
                let merged = span.style.patch(hl_style);
                new_spans.push(Span::styled(
                    text[range.start..range.end].to_string(),
                    merged,
                ));
                pos = range.end;
            }
            if pos < text.len() {
                new_spans.push(Span::styled(text[pos..].to_string(), span.style));
            }
        }
    }

    Line::from(new_spans)
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
            stream: None,
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
        // Phase 8 additions
        assert!(state.sidebar_visible);
        assert!(state.notifications.is_empty());
        assert!(state.filter_history.is_empty());
        assert!(state.filter_history_index.is_none());
        assert!(state.process_detail_index.is_none());
        assert_eq!(state.process_detail_scroll, 0);
        assert!(state.process_detail_sockets.is_none());
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
    fn app_state_detail_scroll_default() {
        let state = AppState::new();
        assert_eq!(state.detail_scroll, 0);
    }

    #[test]
    fn app_mode_entry_detail_variant() {
        let mut state = AppState::new();
        state.mode = AppMode::EntryDetail;
        assert_eq!(state.mode, AppMode::EntryDetail);
        state.mode = AppMode::Normal;
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn app_mode_process_detail_variant() {
        let mut state = AppState::new();
        state.mode = AppMode::ProcessDetail;
        assert_eq!(state.mode, AppMode::ProcessDetail);
        state.mode = AppMode::Normal;
        assert_eq!(state.mode, AppMode::Normal);
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
