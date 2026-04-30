use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::Mutex;

use crate::execution::{EngineHandle, TaskId};
use crate::log::LogEntry;
use crate::log::field_stats::FieldStats;
use crate::log::filter as log_filter;
use crate::log::store::LogStore;
use crate::task::{Registry, TaskDef};

use super::event::run_event_loop;
use super::filter::FilterInputState;
use super::picker::PickerState;
use super::render::{DisplayMode, SourceColors};
use super::search::SearchState;
use super::sidebar::{SidebarEntry, SidebarState};
use super::viewport::ScrollState;

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
    /// Copy menu overlay (choose what to copy)
    CopyMenu,
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
    pub log_lines: Vec<LogEntry>,
    /// The engine's `LogStore`, cloned from `EngineHandle::log_store`.
    pub log_store: Arc<Mutex<LogStore>>,
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
    /// Source visibility filter, keyed by `TaskId`. Empty means show all.
    /// When non-empty, only entries whose source is in this set are shown.
    pub source_filter: HashSet<TaskId>,
    /// Cached sidebar entries, rebuilt each frame from the graph snapshot.
    pub sidebar_entries: Vec<SidebarEntry>,
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
    /// Engine handle. Cloned from the engine started in the binary entry.
    /// `None` only briefly during construction; populated before the
    /// event loop runs.
    pub engine: Option<EngineHandle>,
    /// Id of the most recently launched task. Used by single-task flows
    /// (TUI showing one task, restart-with-`r`) to know what to wait on
    /// or kill.
    pub current_task_id: Option<TaskId>,
    /// Index of the process being viewed in the ProcessDetail overlay.
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
    /// The currently running task definition (kept for restart).
    pub current_task: Option<&'static TaskDef>,
    /// The arguments the current task was launched with (kept for restart).
    pub current_task_args: Vec<String>,
    /// Flag: the event loop should restart the current task.
    pub pending_restart: bool,
    /// Last known viewport height (rows available for log entries), cached for copy operations.
    pub last_viewport_height: Option<u16>,
    /// Shared registry for task discovery and cross-invocation.
    pub registry: Option<Arc<Registry>>,
    /// Per-source field importance statistics for inline display filtering.
    pub field_stats: FieldStats,
    /// Whether to show structured fields inline in log entries.
    pub show_fields: bool,
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
            display_mode: DisplayMode::Preview,
            wrap: false,
            scroll: ScrollState::Tail,
            source_colors: SourceColors::new(),
            sidebar: SidebarState::new(),
            source_filter: HashSet::new(),
            sidebar_entries: Vec::new(),
            filter_input: FilterInputState::new(),
            search: SearchState::new(),
            detail_scroll: 0,
            picker: None,
            all_tasks: Vec::new(),
            group_names: HashMap::new(),
            pending_task: None,
            engine: None,
            current_task_id: None,
            process_detail_index: None,
            process_detail_scroll: 0,
            process_detail_sockets: None,
            sidebar_visible: true,
            notifications: Vec::new(),
            filter_history: Vec::new(),
            filter_history_index: None,
            current_task: None,
            current_task_args: Vec::new(),
            pending_restart: false,
            last_viewport_height: None,
            registry: None,
            field_stats: FieldStats::new(),
            show_fields: true,
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
                if !self.source_filter.is_empty() && !self.source_filter.contains(&entry.source) {
                    return false;
                }
                if let Some(expr) = expr
                    && !log_filter::matches(expr, entry)
                {
                    return false;
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
                if !self.source_filter.is_empty() && !self.source_filter.contains(&entry.source) {
                    return false;
                }
                if let Some(expr) = expr
                    && !log_filter::matches(expr, entry)
                {
                    return false;
                }
                true
            })
            .collect()
    }

    /// Toggle visibility of a source. If source_filter is empty (show all),
    /// switching to filtered mode: add all sources except the toggled one.
    /// If already filtered, toggle the specific source.
    pub fn toggle_source_visibility(&mut self, source: TaskId) {
        if self.source_filter.is_empty() {
            let all_sources: HashSet<TaskId> = self
                .sidebar_entries
                .iter()
                .map(|e| e.source)
                .collect();
            self.source_filter = all_sources;
            self.source_filter.remove(&source);
        } else if self.source_filter.contains(&source) {
            self.source_filter.remove(&source);
        } else {
            self.source_filter.insert(source);
            let all_sources: HashSet<TaskId> = self
                .sidebar_entries
                .iter()
                .map(|e| e.source)
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

    /// Launch a task through the engine. Called from the event loop when a
    /// task is selected from the picker or `r` is used to restart.
    pub async fn launch_picked_task(
        &mut self,
        task: &'static TaskDef,
        task_args: Vec<String>,
    ) {
        self.current_task = Some(task);
        self.current_task_args = task_args.clone();

        if let Some(handle) = self.engine.as_ref() {
            match handle.spawn_task(task, task_args).await {
                Ok(id) => self.current_task_id = Some(id),
                Err(e) => {
                    self.notifications.push((
                        format!("spawn failed: {e}"),
                        std::time::Instant::now(),
                    ));
                }
            }
        }

        self.mode = AppMode::Normal;
        self.picker = None;
        self.pending_task = None;
        self.dirty = true;
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

    /// Create an App that starts with the task picker, attached to an
    /// already-started engine.
    pub fn with_picker(
        tasks: Vec<&'static TaskDef>,
        group_names: HashMap<String, String>,
        registry: Arc<Registry>,
        engine: EngineHandle,
    ) -> Self {
        let picker = PickerState::new(&tasks, &group_names);
        let mut state = AppState::new();
        state.mode = AppMode::TaskPicker;
        state.picker = Some(picker);
        state.all_tasks = tasks;
        state.group_names = group_names;
        state.registry = Some(registry);
        state.log_store = engine.log_store.clone();
        state.engine = Some(engine);
        Self { state }
    }

    /// Create an App configured to run a specific task immediately through
    /// the engine.
    pub async fn with_task(
        task: &'static TaskDef,
        task_args: Vec<String>,
        registry: Arc<Registry>,
        engine: EngineHandle,
    ) -> Self {
        let mut state = AppState::new();
        state.registry = Some(registry);
        state.log_store = engine.log_store.clone();
        state.engine = Some(engine);
        state.launch_picked_task(task, task_args).await;
        Self { state }
    }

    /// Enter the TUI: set up the terminal, run the event loop, and restore
    /// the terminal on exit (including panics). The engine is the caller's
    /// responsibility — `App::run` does not start or stop it.
    pub async fn run(&mut self) -> io::Result<()> {
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
        let _ = std::panic::take_hook();

        result
    }
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
    use ratatui::style::Color;
    use std::collections::HashMap;

    fn make_entry(source: TaskId) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: format!("entry from {source}"),
            parsed: ParsedContent::PlainText,
            source,
            seq: 0,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(format!("entry from {source}")),
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
        assert_eq!(state.display_mode, DisplayMode::Preview);
        assert!(!state.wrap);
        assert_eq!(state.scroll, ScrollState::Tail);
        assert!(!state.sidebar.focused);
        assert_eq!(state.sidebar.selection, 0);
        assert!(state.source_filter.is_empty());
        assert!(state.sidebar_entries.is_empty());
        assert!(state.engine.is_none());
        assert!(state.current_task_id.is_none());
        assert!(state.sidebar_visible);
        assert!(state.notifications.is_empty());
        assert!(state.filter_history.is_empty());
        assert!(state.filter_history_index.is_none());
        assert!(state.process_detail_index.is_none());
        assert_eq!(state.process_detail_scroll, 0);
        assert!(state.process_detail_sockets.is_none());
    }

    #[test]
    fn visible_lines_no_filter() {
        let mut state = AppState::new();
        let api = TaskId(1);
        let worker = TaskId(2);
        state.log_lines.push(make_entry(api));
        state.log_lines.push(make_entry(worker));
        state.log_lines.push(make_entry(api));
        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn visible_lines_with_filter() {
        let mut state = AppState::new();
        let api = TaskId(1);
        let worker = TaskId(2);
        state.log_lines.push(make_entry(api));
        state.log_lines.push(make_entry(worker));
        state.log_lines.push(make_entry(api));
        state.source_filter.insert(api);
        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn show_all_clears_filter() {
        let mut state = AppState::new();
        state.source_filter.insert(TaskId(1));
        assert!(!state.source_filter.is_empty());
        state.show_all_sources();
        assert!(state.source_filter.is_empty());
    }

    #[test]
    fn toggle_source_from_all_visible() {
        let mut state = AppState::new();
        let task = TaskId(10);
        let api = TaskId(11);
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: task,
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: api,
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
            },
        ];
        state.toggle_source_visibility(api);
        assert!(state.source_filter.contains(&task));
        assert!(!state.source_filter.contains(&api));
    }

    #[test]
    fn toggle_source_back_on_clears_filter() {
        let mut state = AppState::new();
        let task = TaskId(10);
        let api = TaskId(11);
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: task,
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: api,
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
            },
        ];
        state.toggle_source_visibility(api);
        assert!(!state.source_filter.is_empty());
        state.toggle_source_visibility(api);
        assert!(state.source_filter.is_empty());
    }
}
