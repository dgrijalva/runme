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
///
/// The picker and quit-confirmation overlays are orthogonal to mode —
/// they're driven by `AppState::picker_open` / `AppState::quit_confirm`
/// rather than mode variants, since they layer over the existing shell
/// instead of replacing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
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
    /// Kill menu overlay (terminate task — `k` chord; design decision 4)
    KillMenu,
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
    /// Base set of sources eligible for display, derived from the focused
    /// sidebar entry via `EngineHandle::source_ids_for`. Empty = show
    /// everything (no filtering applied for "All tasks" focus).
    pub focus_filter: HashSet<TaskId>,
    /// Sources the user has manually toggled off (composes with
    /// `focus_filter`: visible = `(focus_filter or all) ∖ hidden_sources`).
    /// Persists across focus changes — entries that aren't part of any
    /// current `focus_filter` simply stay hidden silently.
    pub hidden_sources: HashSet<TaskId>,
    /// Cached sidebar entries, rebuilt each frame from the graph snapshot.
    pub sidebar_entries: Vec<SidebarEntry>,
    /// Filter input state for the filter bar.
    pub filter_input: FilterInputState,
    /// Search state for / search with n/N navigation.
    pub search: SearchState,
    /// Scroll offset within the entry detail overlay (for long entries).
    pub detail_scroll: usize,
    /// Task picker state. `Some` whenever `picker_open` is true; the
    /// picker is an overlay layered on the Normal-mode shell rather than
    /// a mode of its own.
    pub picker: Option<PickerState>,
    /// Whether the picker overlay is currently visible. Re-entrant from
    /// Normal mode via `t`; toggles independently of `mode`.
    pub picker_open: bool,
    /// Whether the quit-confirmation modal is visible. Set by `q` when
    /// any task other than root is in `Setup`/`Ready` status.
    pub quit_confirm: bool,
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
            focus_filter: HashSet::new(),
            hidden_sources: HashSet::new(),
            sidebar_entries: Vec::new(),
            filter_input: FilterInputState::new(),
            search: SearchState::new(),
            detail_scroll: 0,
            picker: None,
            picker_open: false,
            quit_confirm: false,
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

    /// Whether a given source is currently visible.
    ///
    /// Visible = (in `focus_filter` OR `focus_filter` is empty)
    ///        AND (NOT in `hidden_sources`).
    fn source_visible(&self, source: TaskId) -> bool {
        if !self.focus_filter.is_empty() && !self.focus_filter.contains(&source) {
            return false;
        }
        if self.hidden_sources.contains(&source) {
            return false;
        }
        true
    }

    /// Get the visible log lines as indices into `self.log_lines`.
    /// Filters by focus + manual hides + expression filter.
    pub fn visible_line_indices(&self) -> Vec<usize> {
        let expr = self.filter_input.active_expr();
        self.log_lines
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                if !self.source_visible(entry.source) {
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

    /// Get the filtered log lines (focus + manual hides + expression filter).
    pub fn visible_log_lines(&self) -> Vec<&LogEntry> {
        let expr = self.filter_input.active_expr();
        self.log_lines
            .iter()
            .filter(|entry| {
                if !self.source_visible(entry.source) {
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

    /// Toggle a manual hide for a source. Composes with focus filter:
    /// `hidden_sources` is independent of `focus_filter`. Re-toggling
    /// removes the source from `hidden_sources` (un-hides).
    pub fn toggle_source_visibility(&mut self, source: TaskId) {
        if self.hidden_sources.contains(&source) {
            self.hidden_sources.remove(&source);
        } else {
            self.hidden_sources.insert(source);
        }
    }

    /// Show all sources: clears manual hides only. The focus filter is
    /// driven by sidebar selection and isn't affected here.
    pub fn show_all_sources(&mut self) {
        self.hidden_sources.clear();
    }

    /// Update the focus filter based on the currently focused sidebar entry.
    ///
    /// - "All tasks" (root) => empty `focus_filter` (no filtering by focus).
    /// - Task entry => `source_ids_for(task_id)` (the task and its subtree).
    /// - Process entry => single-source filter `{process_id}`.
    pub fn refresh_focus_filter(&mut self) {
        let Some(entry) = self.sidebar_entries.get(self.sidebar.selection) else {
            self.focus_filter.clear();
            return;
        };
        let Some(handle) = self.engine.as_ref() else {
            self.focus_filter.clear();
            return;
        };
        if entry.source == handle.root {
            // "All tasks" — no focus filter.
            self.focus_filter.clear();
            return;
        }
        if !entry.is_task {
            // Process entry: filter to just this process's logs.
            self.focus_filter = std::iter::once(entry.source).collect();
            return;
        }
        let ids = handle.source_ids_for(entry.source);
        self.focus_filter = ids.into_iter().collect();
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
        self.picker_open = false;
        self.pending_task = None;
        self.dirty = true;
    }

    /// Open the task picker overlay. Re-entrant from Normal mode at any
    /// time (decision 1 + 8). Builds picker state from the cached task
    /// list / group names.
    pub fn open_picker(&mut self) {
        if self.all_tasks.is_empty() {
            return;
        }
        self.picker = Some(PickerState::new(&self.all_tasks, &self.group_names));
        self.picker_open = true;
        self.dirty = true;
    }

    /// Close the picker overlay without launching a task.
    pub fn close_picker(&mut self) {
        self.picker = None;
        self.picker_open = false;
        self.dirty = true;
    }

    /// Whether any task other than the synthetic root is currently in a
    /// running state (`Setup` or `Ready`).
    pub fn has_running_tasks(&self) -> bool {
        let Some(handle) = self.engine.as_ref() else {
            return false;
        };
        let snapshot = handle.graph.borrow().clone();
        for (id, node) in snapshot.tasks.iter() {
            if *id == handle.root {
                continue;
            }
            if matches!(
                node.status,
                crate::execution::TaskStatus::Setup | crate::execution::TaskStatus::Ready
            ) {
                return true;
            }
        }
        false
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

    /// Create an App that starts with the task picker overlay open over
    /// an empty Normal-mode shell, attached to an already-started engine.
    pub fn with_picker(
        tasks: Vec<&'static TaskDef>,
        group_names: HashMap<String, String>,
        registry: Arc<Registry>,
        engine: EngineHandle,
    ) -> Self {
        let picker = PickerState::new(&tasks, &group_names);
        let mut state = AppState::new();
        state.mode = AppMode::Normal;
        state.picker = Some(picker);
        state.picker_open = true;
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
        assert!(state.focus_filter.is_empty());
        assert!(state.hidden_sources.is_empty());
        assert!(state.sidebar_entries.is_empty());
        assert!(!state.picker_open);
        assert!(!state.quit_confirm);
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
    fn visible_lines_with_focus_filter() {
        let mut state = AppState::new();
        let api = TaskId(1);
        let worker = TaskId(2);
        state.log_lines.push(make_entry(api));
        state.log_lines.push(make_entry(worker));
        state.log_lines.push(make_entry(api));
        state.focus_filter.insert(api);
        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![0, 2]);
    }

    #[test]
    fn visible_lines_with_hidden_sources() {
        let mut state = AppState::new();
        let api = TaskId(1);
        let worker = TaskId(2);
        state.log_lines.push(make_entry(api));
        state.log_lines.push(make_entry(worker));
        state.log_lines.push(make_entry(api));
        state.hidden_sources.insert(api);
        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![1]);
    }

    #[test]
    fn focus_and_hide_compose() {
        // Focus filter limits to {api, worker}; manual hide removes worker.
        // Result: only api entries visible.
        let mut state = AppState::new();
        let api = TaskId(1);
        let worker = TaskId(2);
        let other = TaskId(3);
        state.log_lines.push(make_entry(api));
        state.log_lines.push(make_entry(worker));
        state.log_lines.push(make_entry(other));
        state.focus_filter.insert(api);
        state.focus_filter.insert(worker);
        state.hidden_sources.insert(worker);
        let indices = state.visible_line_indices();
        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn show_all_clears_hidden_sources() {
        let mut state = AppState::new();
        state.hidden_sources.insert(TaskId(1));
        assert!(!state.hidden_sources.is_empty());
        state.show_all_sources();
        assert!(state.hidden_sources.is_empty());
    }

    #[test]
    fn show_all_does_not_clear_focus_filter() {
        let mut state = AppState::new();
        state.focus_filter.insert(TaskId(1));
        state.hidden_sources.insert(TaskId(2));
        state.show_all_sources();
        assert!(state.hidden_sources.is_empty());
        assert!(state.focus_filter.contains(&TaskId(1)));
    }

    #[test]
    fn toggle_source_visibility_adds_then_removes() {
        let mut state = AppState::new();
        let api = TaskId(11);
        state.toggle_source_visibility(api);
        assert!(state.hidden_sources.contains(&api));
        state.toggle_source_visibility(api);
        assert!(!state.hidden_sources.contains(&api));
    }

    #[test]
    fn focus_change_does_not_reset_hidden_sources() {
        // Hidden sources persist across focus changes (silently inert if
        // not in the new focus set).
        let mut state = AppState::new();
        let api = TaskId(11);
        state.hidden_sources.insert(api);
        state.focus_filter = [TaskId(99)].into_iter().collect();
        // Manually re-derive — refresh_focus_filter would need a real engine.
        assert!(state.hidden_sources.contains(&api));
    }

    #[test]
    fn open_picker_with_empty_tasks_is_noop() {
        let mut state = AppState::new();
        state.open_picker();
        assert!(!state.picker_open);
        assert!(state.picker.is_none());
    }

    #[test]
    fn close_picker_clears_state() {
        let mut state = AppState::new();
        state.picker_open = true;
        state.picker = Some(super::super::picker::PickerState::new(
            &[],
            &HashMap::new(),
        ));
        state.close_picker();
        assert!(!state.picker_open);
        assert!(state.picker.is_none());
    }

    #[test]
    fn has_running_tasks_without_engine_is_false() {
        let state = AppState::new();
        assert!(!state.has_running_tasks());
    }
}
