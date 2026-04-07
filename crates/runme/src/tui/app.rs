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
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use tokio::sync::Mutex;

use crate::log::LogEntry;
use crate::log::store::LogStore;
use crate::task::TaskDef;

use super::event::run_event_loop;
use super::render::{DisplayMode, SourceColors};
use super::runner::{TaskRunner, TaskStatus};
use super::viewport::{self, ScrollState, new_entries_since_pin};

/// The mode the application is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Log viewer, navigating with keyboard
    Normal,
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
        }
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

        runner.launch(task);

        let mut state = AppState::new();
        state.log_store = log_store;
        state.task_status = Some(task_status);
        state.task_name = Some(task.name.to_string());

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

/// Render a single frame. Draws log lines in the main area and a status bar
/// at the bottom showing mode, task name, task status, and scroll position.
pub fn render_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        // Vertical layout: main area (fills) + status bar (1 line)
        let chunks = Layout::vertical([
            Constraint::Min(0),    // main area
            Constraint::Length(1), // status bar
        ])
        .split(area);

        let main_area = chunks[0];
        let main_width = main_area.width;
        let main_height = main_area.height;

        // Main area: use viewport-driven rendering
        let lines: Vec<Line> = if state.log_lines.is_empty() {
            if state.task_name.is_some() {
                vec![Line::from(Span::styled(
                    "  Waiting for output...",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                vec![Line::from(Span::styled(
                    "  No task running. Press q to quit.",
                    Style::default().fg(Color::DarkGray),
                ))]
            }
        } else {
            // Use the viewport to compute which entries are visible
            let vp_layout = viewport::layout(
                &state.scroll,
                &state.log_lines,
                main_height,
                main_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );

            // Build a line buffer for the entire viewport, initialized to empty
            let mut line_buffer: Vec<Line<'static>> = (0..main_height)
                .map(|_| Line::from(""))
                .collect();

            // Place rendered entries into the buffer at their Y positions
            let cursor_style = Style::default().bg(Color::DarkGray);
            for ve in &vp_layout.entries {
                for (line_offset, line) in ve.lines.iter().enumerate() {
                    let y = ve.y as usize + line_offset;
                    if y < main_height as usize {
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
        frame.render_widget(log_paragraph, main_area);

        // Status bar
        let mode_text = match state.mode {
            AppMode::Normal => "NORMAL",
        };

        // Build status line with task info
        let mut spans = vec![
            Span::styled(" runme ", Style::default().fg(Color::Black).bg(Color::Cyan)),
            Span::raw(" "),
            Span::styled(
                format!(" {} ", mode_text),
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

        // Scroll position / entry count
        if !state.log_lines.is_empty() {
            spans.push(Span::raw(" "));
            match state.scroll {
                ScrollState::Tail => {
                    spans.push(Span::styled(
                        format!(" TAIL | {} ", state.log_lines.len()),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ScrollState::Pinned { cursor, .. } => {
                    let total = state.log_lines.len();
                    let new_count = new_entries_since_pin(&state.scroll, total);
                    let pos_text = if new_count > 0 {
                        format!(" {} / {} (+{} new) ", cursor + 1, total, new_count)
                    } else {
                        format!(" {} / {} ", cursor + 1, total)
                    };
                    spans.push(Span::styled(pos_text, Style::default().fg(Color::DarkGray)));
                }
            }
        }

        let status_line = Line::from(spans);

        let status_bar = Paragraph::new(status_line)
            .style(Style::default().bg(Color::DarkGray).fg(Color::White));

        frame.render_widget(status_bar, chunks[1]);
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
}
