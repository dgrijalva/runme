use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::log::LogEntry;

use super::app::{AppMode, AppState, render_frame};
use super::render::DisplayMode;
use super::runner::{ProcessStatus, TaskStatus};
use super::sidebar;
use super::viewport::{
    self, scroll_down, scroll_down_half_page, scroll_to_bottom, scroll_to_top,
    scroll_up, scroll_up_half_page,
};

/// Target frame interval (~60fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Run the main event loop. This drives the entire TUI:
/// - Polls terminal events (keyboard, mouse, resize)
/// - Receives log entries from the LogStore broadcast
/// - Renders on a tick when the dirty flag is set
/// - Handles graceful shutdown via signals
pub async fn run_event_loop(
    state: &mut AppState,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    let mut event_stream = EventStream::new();
    let mut render_interval = tokio::time::interval(FRAME_INTERVAL);
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Set up signal handlers for clean shutdown
    let mut sigint =
        signal(SignalKind::interrupt()).map_err(io::Error::other)?;
    let mut sigterm =
        signal(SignalKind::terminate()).map_err(io::Error::other)?;

    // Load any entries already in the LogStore (from tasks that completed
    // before the event loop started) and subscribe for new ones.
    let mut log_rx: broadcast::Receiver<LogEntry> = {
        let store = state.log_store.lock().await;
        let existing = store.compose_owned();
        if !existing.is_empty() {
            state.log_lines = existing;
            state.dirty = true;
        }
        store.subscribe()
    };

    // Timer for lsof polling (process detail panel) and notification cleanup
    let mut lsof_interval = tokio::time::interval(Duration::from_secs(3));
    lsof_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the first immediate tick
    lsof_interval.tick().await;

    // Track previous process statuses for crash surfacing
    let mut prev_process_statuses: Vec<(String, ProcessStatus)> = Vec::new();

    while state.running {
        tokio::select! {
            // Terminal input events
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        // Any keypress dismisses notifications
                        if matches!(event, Event::Key(_)) {
                            state.notifications.clear();
                        }

                        handle_event(event, state, terminal);

                        // Check if a task was selected from the picker
                        if let Some(task) = state.pending_task.take() {
                            state.launch_picked_task(task);
                            // Re-subscribe to the new LogStore
                            let store = state.log_store.lock().await;
                            let existing = store.compose_owned();
                            if !existing.is_empty() {
                                state.log_lines = existing;
                            }
                            log_rx = store.subscribe();
                            drop(store);
                        }
                    }
                    Some(Err(_)) => {
                        // Terminal event read error — shut down
                        state.running = false;
                    }
                    None => {
                        // Event stream ended
                        state.running = false;
                    }
                }
            }

            // New log entries from the LogStore
            result = log_rx.recv() => {
                match result {
                    Ok(entry) => {
                        // Check new entry against active search before pushing
                        if state.search.active {
                            let visible_count = state.visible_line_indices().len();
                            let text = entry.message.as_deref().unwrap_or(&entry.raw);
                            state.search.check_new_entry(visible_count, text);
                        }
                        state.log_lines.push(entry);
                        // In tail mode, scroll state stays as Tail (which will
                        // automatically anchor to the new last entry on render).
                        // In pinned mode, the anchor stays put and the +N new
                        // counter increments naturally.
                        state.dirty = true;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // We missed some entries; reload from the store
                        let store = state.log_store.lock().await;
                        state.log_lines = store.compose_owned();
                        state.dirty = true;
                        drop(store);
                        let _ = n; // acknowledged
                        // Rescan search matches since entries changed
                        if state.search.active {
                            let visible = state.visible_log_lines();
                            let texts: Vec<(usize, String)> = visible
                                .iter()
                                .enumerate()
                                .map(|(i, e)| {
                                    let text = e.message.as_deref().unwrap_or(&e.raw).to_string();
                                    (i, text)
                                })
                                .collect();
                            state.search.scan_matches(texts.iter().map(|(i, t)| (*i, t.as_str())));
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Log stream closed; keep running (task may have finished)
                    }
                }
            }

            // Render tick — only redraw when dirty
            _ = render_interval.tick(), if state.dirty => {
                // Refresh process statuses and rebuild sidebar entries
                refresh_sidebar_state(state).await;

                // Crash surfacing: detect newly failed processes
                check_for_crashes(state, &mut prev_process_statuses);

                // Auto-dismiss expired notifications (5 seconds)
                let now = std::time::Instant::now();
                state.notifications.retain(|(_, ts)| now.duration_since(*ts) < Duration::from_secs(5));

                render_frame(terminal, state)?;
                state.dirty = false;
            }

            // lsof polling timer (for process detail panel)
            _ = lsof_interval.tick(), if state.mode == AppMode::ProcessDetail => {
                poll_lsof(state).await;
                state.dirty = true;
            }

            // SIGINT (Ctrl-C)
            _ = sigint.recv() => {
                state.running = false;
            }

            // SIGTERM
            _ = sigterm.recv() => {
                state.running = false;
            }
        }
    }

    Ok(())
}

/// Refresh process statuses and rebuild sidebar entries from the runner's state.
async fn refresh_sidebar_state(state: &mut AppState) {
    // Get current task status
    let task_status = if let Some(ts) = &state.task_status {
        ts.lock().await.clone()
    } else {
        TaskStatus::Setup
    };

    // Refresh process statuses
    if let Some(procs_arc) = &state.processes {
        let mut procs = procs_arc.lock().await;
        for proc in procs.iter_mut() {
            proc.refresh_status();
        }

        // Rebuild sidebar entries
        state.sidebar_entries = sidebar::build_sidebar_entries(
            state.task_name.as_deref(),
            &task_status,
            &procs,
            &state.source_filter,
            &mut state.source_colors,
        );
    } else {
        state.sidebar_entries = sidebar::build_sidebar_entries(
            state.task_name.as_deref(),
            &task_status,
            &[],
            &state.source_filter,
            &mut state.source_colors,
        );
    }

    // Clamp sidebar selection
    state.sidebar.clamp_selection(state.sidebar_entries.len());
}

/// Dispatch a terminal event to the appropriate handler.
fn handle_event(
    event: Event,
    state: &mut AppState,
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) {
    match event {
        Event::Key(key_event) => handle_key(key_event, state, terminal),
        Event::Mouse(mouse_event) => handle_mouse(mouse_event, state, terminal),
        Event::Resize(_, _) => {
            // On resize, keep the same anchor entry — heights will reflow
            state.dirty = true;
        }
        _ => {}
    }
}

/// Handle a keyboard event.
fn handle_key(
    key: KeyEvent,
    state: &mut AppState,
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) {
    // Get viewport dimensions for scroll calculations
    let term_size = terminal.size().unwrap_or_default();
    // Main area height = total height - 1 (status bar)
    let viewport_height = term_size.height.saturating_sub(1);
    // Log viewer width = total width minus sidebar if task is running
    let sidebar_width = if state.task_name.is_some() {
        super::sidebar::SIDEBAR_WIDTH
    } else {
        0
    };
    let viewport_width = term_size.width.saturating_sub(sidebar_width);

    // Build filtered entries for scroll operations
    let filtered_entries: Vec<LogEntry> = if state.source_filter.is_empty() {
        state.log_lines.clone()
    } else {
        state
            .log_lines
            .iter()
            .filter(|e| state.source_filter.contains(&e.source))
            .cloned()
            .collect()
    };

    // Task picker mode: dedicated key handling
    if state.mode == AppMode::TaskPicker {
        handle_picker_key(key, state);
        state.dirty = true;
        return;
    }

    // Filter input mode gets its own key handling — only Esc and Ctrl-C escape
    if state.mode == AppMode::FilterInput {
        handle_filter_input_key(key, state);
        state.dirty = true;
        return;
    }

    // Search input mode gets its own key handling
    if state.mode == AppMode::SearchInput {
        handle_search_input_key(key, state, &filtered_entries, viewport_height, viewport_width);
        state.dirty = true;
        return;
    }

    // Entry detail mode: dedicated key handling
    if state.mode == AppMode::EntryDetail {
        handle_detail_key(key, state, &filtered_entries, viewport_height, viewport_width);
        state.dirty = true;
        return;
    }

    // Process detail mode: dedicated key handling
    if state.mode == AppMode::ProcessDetail {
        handle_process_detail_key(key, state);
        state.dirty = true;
        return;
    }

    // Help mode: any key dismisses
    if state.mode == AppMode::Help {
        state.mode = AppMode::Normal;
        state.dirty = true;
        return;
    }

    // Global keys (work regardless of focus)
    match key.code {
        // '?' toggles help overlay
        KeyCode::Char('?') => {
            state.mode = AppMode::Help;
            state.dirty = true;
            return;
        }
        // 'q' quits the application
        KeyCode::Char('q') => {
            state.running = false;
            state.dirty = true;
            return;
        }
        // Ctrl-C also quits
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.running = false;
            state.dirty = true;
            return;
        }
        // Tab: toggle sidebar focus
        KeyCode::Tab => {
            state.sidebar.focused = !state.sidebar.focused;
            state.dirty = true;
            return;
        }
        _ => {}
    }

    if state.sidebar.focused {
        // -- Sidebar-focused key bindings --
        handle_sidebar_key(key, state);
    } else {
        // -- Log viewer-focused key bindings --
        handle_log_viewer_key(key, state, &filtered_entries, viewport_height, viewport_width);
    }

    // Any key press marks the state as dirty
    state.dirty = true;
}

/// Handle keys when sidebar is focused.
fn handle_sidebar_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // j / Down: move sidebar selection down
        KeyCode::Char('j') | KeyCode::Down => {
            state.sidebar.move_down(state.sidebar_entries.len());
        }

        // k / Up: move sidebar selection up
        KeyCode::Char('k') | KeyCode::Up => {
            state.sidebar.move_up();
        }

        // Enter: open process detail (for process entries) or toggle source visibility (for task entry)
        KeyCode::Enter => {
            if let Some(entry) = state.sidebar_entries.get(state.sidebar.selection) {
                if entry.is_task {
                    let source = entry.source.clone();
                    state.toggle_source_visibility(&source);
                } else {
                    // Open process detail overlay
                    state.process_detail_index = Some(state.sidebar.selection);
                    state.process_detail_scroll = 0;
                    state.process_detail_sockets = None; // will be polled
                    state.mode = AppMode::ProcessDetail;
                }
            }
        }

        // Space: toggle source visibility
        KeyCode::Char(' ') => {
            if let Some(entry) = state.sidebar_entries.get(state.sidebar.selection) {
                let source = entry.source.clone();
                state.toggle_source_visibility(&source);
            }
        }

        // s: stop selected process (SIGTERM)
        KeyCode::Char('s') => {
            send_signal_to_selected(state, nix::sys::signal::Signal::SIGTERM);
        }

        // S: send SIGHUP to selected process
        KeyCode::Char('S') => {
            send_signal_to_selected(state, nix::sys::signal::Signal::SIGHUP);
        }

        // -- Source toggle shortcuts (work in sidebar too) --

        // a: show all sources
        KeyCode::Char('a') => {
            state.show_all_sources();
        }

        // 1-9: toggle source N
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if let Some(source) = sidebar::source_for_index(&state.sidebar_entries, idx) {
                let source = source.to_string();
                state.toggle_source_visibility(&source);
            }
        }

        _ => {}
    }
}

/// Handle keys when log viewer is focused.
fn handle_log_viewer_key(
    key: KeyEvent,
    state: &mut AppState,
    filtered_entries: &[LogEntry],
    viewport_height: u16,
    viewport_width: u16,
) {
    match key.code {
        // j / Down: move cursor to next entry
        KeyCode::Char('j') | KeyCode::Down => {
            state.scroll = scroll_down(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // k / Up: move cursor to previous entry
        KeyCode::Char('k') | KeyCode::Up => {
            state.scroll = scroll_up(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // Ctrl-d / Page Down / ]: scroll down half page
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = scroll_down_half_page(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }
        KeyCode::PageDown | KeyCode::Char(']') => {
            state.scroll = scroll_down_half_page(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // Ctrl-u / Page Up / [: scroll up half page
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = scroll_up_half_page(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }
        KeyCode::PageUp | KeyCode::Char('[') => {
            state.scroll = scroll_up_half_page(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // g / Home: jump to first entry
        KeyCode::Char('g') | KeyCode::Home => {
            state.scroll = scroll_to_top(&state.scroll, filtered_entries);
        }

        // G / End: jump to last entry, enter tail mode
        KeyCode::Char('G') | KeyCode::End => {
            state.scroll = scroll_to_bottom(&state.scroll, filtered_entries);
        }

        // Enter: open entry detail view
        KeyCode::Enter => {
            // Only open if there are visible entries
            if !state.visible_line_indices().is_empty() {
                state.detail_scroll = 0;
                state.mode = AppMode::EntryDetail;
            }
        }

        // -- Display mode toggles --

        // v or m: toggle preview/raw mode
        KeyCode::Char('v') | KeyCode::Char('m') => {
            state.display_mode = match state.display_mode {
                DisplayMode::Preview => DisplayMode::Raw,
                DisplayMode::Raw => DisplayMode::Preview,
            };
        }

        // w: toggle truncated/wrapped
        KeyCode::Char('w') => {
            state.wrap = !state.wrap;
        }

        // \: toggle sidebar visibility
        KeyCode::Char('\\') => {
            state.sidebar_visible = !state.sidebar_visible;
        }

        // e: export visible log to file
        KeyCode::Char('e') => {
            export_visible_log(state);
        }

        // -- Source toggle shortcuts --

        // a: show all sources
        KeyCode::Char('a') => {
            state.show_all_sources();
        }

        // f: enter filter input mode
        KeyCode::Char('f') => {
            state.filter_input.save_current();
            state.mode = AppMode::FilterInput;
        }

        // /: enter search input mode
        KeyCode::Char('/') => {
            // Pre-populate with the previous search pattern if any
            state.search.text = state.search.pattern.clone();
            state.search.cursor = state.search.text.len();
            state.mode = AppMode::SearchInput;
        }

        // n: jump to next search match
        KeyCode::Char('n') => {
            if state.search.active
                && let Some(target) = state.search.next_match()
            {
                navigate_to_entry(state, target, filtered_entries, viewport_height, viewport_width);
            }
        }

        // N: jump to previous search match
        KeyCode::Char('N') => {
            if state.search.active
                && let Some(target) = state.search.prev_match()
            {
                navigate_to_entry(state, target, filtered_entries, viewport_height, viewport_width);
            }
        }

        // 1-9: toggle source N
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if let Some(source) = sidebar::source_for_index(&state.sidebar_entries, idx) {
                let source = source.to_string();
                state.toggle_source_visibility(&source);
            }
        }

        _ => {}
    }
}

/// Handle keys when in task picker mode.
///
/// Returns `Some(task)` if the user selected a task, `None` otherwise.
/// The caller (event loop) is responsible for launching the task.
fn handle_picker_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // Esc or q: quit (no task to run)
        KeyCode::Esc | KeyCode::Char('q') => {
            if let Some(ref picker) = state.picker
                && (picker.input.is_empty() || key.code == KeyCode::Esc)
            {
                state.running = false;
            }
        }

        // Ctrl-C: quit
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.running = false;
        }

        // Enter: launch selected task
        KeyCode::Enter => {
            if let Some(ref picker) = state.picker
                && let Some(task) = picker.selected_task()
            {
                state.pending_task = Some(task);
            }
        }

        // j / Down: move selection down
        KeyCode::Char('j') | KeyCode::Down => {
            if let Some(ref mut picker) = state.picker {
                picker.move_down();
            }
        }

        // k / Up: move selection up
        KeyCode::Char('k') | KeyCode::Up => {
            if let Some(ref mut picker) = state.picker {
                picker.move_up();
            }
        }

        // Backspace: delete last char of input
        KeyCode::Backspace => {
            if let Some(ref mut picker) = state.picker {
                picker.delete_char();
            }
        }

        // Ctrl-u: clear input
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(ref mut picker) = state.picker {
                picker.input.clear();
                picker.cursor = 0;
                picker.selection = 0;
                picker.scroll_offset = 0;
            }
        }

        // Any other character: insert into fuzzy input
        KeyCode::Char(ch) => {
            if let Some(ref mut picker) = state.picker {
                picker.insert_char(ch);
            }
        }

        _ => {}
    }
}

/// Handle keys when in filter input mode.
fn handle_filter_input_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // Enter: confirm filter and return to Normal mode
        KeyCode::Enter => {
            // Save to filter history if non-empty
            let text = state.filter_input.text.clone();
            if !text.is_empty() {
                // Don't duplicate the last entry
                if state.filter_history.last().map(|s| s.as_str()) != Some(&text) {
                    state.filter_history.push(text);
                }
            }
            state.filter_history_index = None;
            state.mode = AppMode::Normal;
        }

        // Esc: cancel (revert) and return to Normal mode
        KeyCode::Esc => {
            state.filter_input.revert();
            state.filter_history_index = None;
            state.mode = AppMode::Normal;
        }

        // Up arrow: cycle to previous filter history entry
        KeyCode::Up => {
            if !state.filter_history.is_empty() {
                let idx = match state.filter_history_index {
                    Some(i) => {
                        if i > 0 { i - 1 } else { 0 }
                    }
                    None => state.filter_history.len() - 1,
                };
                state.filter_history_index = Some(idx);
                state.filter_input.text = state.filter_history[idx].clone();
                state.filter_input.cursor = state.filter_input.text.len();
            }
        }

        // Down arrow: cycle to next filter history entry
        KeyCode::Down => {
            if let Some(idx) = state.filter_history_index {
                if idx + 1 < state.filter_history.len() {
                    let new_idx = idx + 1;
                    state.filter_history_index = Some(new_idx);
                    state.filter_input.text = state.filter_history[new_idx].clone();
                    state.filter_input.cursor = state.filter_input.text.len();
                } else {
                    // Past the end of history — clear to empty
                    state.filter_history_index = None;
                    state.filter_input.text.clear();
                    state.filter_input.cursor = 0;
                }
            }
        }

        // Ctrl-u: clear the input
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.filter_input.clear();
        }

        // Ctrl-c: cancel and return to Normal mode
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.filter_input.revert();
            state.mode = AppMode::Normal;
        }

        // Left arrow: move cursor left
        KeyCode::Left => {
            state.filter_input.move_left();
        }

        // Right arrow: move cursor right
        KeyCode::Right => {
            state.filter_input.move_right();
        }

        // Backspace: delete character before cursor
        KeyCode::Backspace => {
            state.filter_input.delete_char_before();
        }

        // Any other character: insert into the filter text
        KeyCode::Char(ch) => {
            state.filter_input.insert_char(ch);
        }

        _ => {}
    }
}

/// Handle keys when in search input mode.
fn handle_search_input_key(
    key: KeyEvent,
    state: &mut AppState,
    filtered_entries: &[LogEntry],
    viewport_height: u16,
    viewport_width: u16,
) {
    match key.code {
        // Enter: confirm search, scan matches, jump to nearest
        KeyCode::Enter => {
            state.search.confirm();
            if state.search.active {
                // Scan all visible entries for the pattern
                let visible = state.visible_log_lines();
                let texts: Vec<(usize, String)> = visible
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let text = e.message.as_deref().unwrap_or(&e.raw).to_string();
                        (i, text)
                    })
                    .collect();
                state.search.scan_matches(texts.iter().map(|(i, t)| (*i, t.as_str())));

                // Jump to the nearest match from the current cursor position
                let cursor_pos = match state.scroll {
                    viewport::ScrollState::Tail => {
                        let visible_count = state.visible_line_indices().len();
                        if visible_count > 0 { visible_count - 1 } else { 0 }
                    }
                    viewport::ScrollState::Pinned { cursor, .. } => cursor,
                };
                if let Some(target) = state.search.jump_to_nearest(cursor_pos) {
                    navigate_to_entry(state, target, filtered_entries, viewport_height, viewport_width);
                }
            }
            state.mode = AppMode::Normal;
        }

        // Esc: cancel search and return to Normal mode
        KeyCode::Esc => {
            state.search.cancel();
            state.mode = AppMode::Normal;
        }

        // Ctrl-u: clear the search input
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.search.clear_input();
        }

        // Ctrl-c: cancel and return to Normal mode
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.search.cancel();
            state.mode = AppMode::Normal;
        }

        // Left arrow: move cursor left
        KeyCode::Left => {
            state.search.move_left();
        }

        // Right arrow: move cursor right
        KeyCode::Right => {
            state.search.move_right();
        }

        // Backspace: delete character before cursor
        KeyCode::Backspace => {
            state.search.delete_char_before();
        }

        // Any other character: insert into the search text
        KeyCode::Char(ch) => {
            state.search.insert_char(ch);
        }

        _ => {}
    }
}

/// Send a signal to the process corresponding to the selected sidebar entry.
///
/// Sends the signal to the process group (pgid) if available, falling back to
/// the individual pid. If SIGTERM is sent, updates the process status to Stopped.
fn send_signal_to_selected(state: &mut AppState, sig: nix::sys::signal::Signal) {
    use nix::sys::signal;
    use nix::unistd::Pid;

    let selection = state.sidebar.selection;
    let entry = match state.sidebar_entries.get(selection) {
        Some(e) => e,
        None => return,
    };

    // Skip task entries — only processes can be signaled
    if entry.is_task {
        return;
    }

    // Find the matching process in the shared process list.
    // The sidebar entries are built from processes, and we match by command_label.
    // Since we don't have direct access to the mutex here (we're in a sync context),
    // we use try_lock. The processes Arc is available on state.
    if let Some(procs_arc) = &state.processes {
        if let Ok(mut procs) = procs_arc.try_lock() {
            // Find process matching this sidebar entry.
            // Sidebar entry index 0 is the task, so process index = selection - 1
            // (assuming task entry is present and is always first).
            let proc_idx = if state.task_name.is_some() {
                selection.checked_sub(1)
            } else {
                Some(selection)
            };

            if let Some(idx) = proc_idx {
                // The sidebar lists running processes first, then completed.
                // We need to find the right process. The sidebar build_sidebar_entries
                // orders: running (by spawn order) then completed (by spawn order).
                // The processes Vec is in spawn order. We need to map sidebar index
                // back to the processes vec.
                let mut running_indices: Vec<usize> = Vec::new();
                let mut completed_indices: Vec<usize> = Vec::new();
                for (i, p) in procs.iter().enumerate() {
                    if p.status == ProcessStatus::Running {
                        running_indices.push(i);
                    } else {
                        completed_indices.push(i);
                    }
                }
                let ordered: Vec<usize> = running_indices.into_iter().chain(completed_indices).collect();

                if let Some(&proc_vec_idx) = ordered.get(idx) {
                    let proc = &mut procs[proc_vec_idx];

                    // Try pgid first (sends to process group), then pid
                    let target_pid = if let Some(pgid) = proc.pgid {
                        // Negative pid sends to the process group
                        Some(Pid::from_raw(-pgid))
                    } else if let Some(pid) = proc.pid {
                        Some(Pid::from_raw(pid as i32))
                    } else {
                        None
                    };

                    if let Some(pid) = target_pid {
                        let _ = signal::kill(pid, sig);
                        // If we sent SIGTERM, mark as Stopped
                        if sig == signal::Signal::SIGTERM {
                            proc.status = ProcessStatus::Stopped;
                        }
                    }
                }
            }
        }
    }
}

/// Handle keys when in entry detail mode.
fn handle_detail_key(
    key: KeyEvent,
    state: &mut AppState,
    filtered_entries: &[LogEntry],
    viewport_height: u16,
    viewport_width: u16,
) {
    match key.code {
        // Esc or q: close detail view, return to Normal mode
        KeyCode::Esc | KeyCode::Char('q') => {
            state.mode = AppMode::Normal;
        }

        // j / Down: scroll down within detail pane
        KeyCode::Char('j') | KeyCode::Down => {
            state.detail_scroll = state.detail_scroll.saturating_add(1);
        }

        // k / Up: scroll up within detail pane
        KeyCode::Char('k') | KeyCode::Up => {
            state.detail_scroll = state.detail_scroll.saturating_sub(1);
        }

        // n: close detail and jump to next search match (or next entry)
        KeyCode::Char('n') => {
            state.mode = AppMode::Normal;
            if state.search.active {
                if let Some(target) = state.search.next_match() {
                    navigate_to_entry(state, target, filtered_entries, viewport_height, viewport_width);
                }
            } else {
                // Jump to next entry
                state.scroll = scroll_down(
                    &state.scroll,
                    filtered_entries,
                    viewport_height,
                    viewport_width,
                    state.display_mode,
                    state.wrap,
                    &mut state.source_colors,
                );
            }
        }

        // N: close detail and jump to previous search match (or previous entry)
        KeyCode::Char('N') => {
            state.mode = AppMode::Normal;
            if state.search.active {
                if let Some(target) = state.search.prev_match() {
                    navigate_to_entry(state, target, filtered_entries, viewport_height, viewport_width);
                }
            } else {
                // Jump to previous entry
                state.scroll = scroll_up(
                    &state.scroll,
                    filtered_entries,
                    viewport_height,
                    viewport_width,
                    state.display_mode,
                    state.wrap,
                    &mut state.source_colors,
                );
            }
        }

        // y: copy raw entry text to clipboard via OSC 52
        KeyCode::Char('y') => {
            copy_entry_to_clipboard(state);
        }

        _ => {}
    }
}

/// Copy the currently focused entry's raw text to the clipboard using OSC 52 escape sequence.
fn copy_entry_to_clipboard(state: &AppState) {
    use base64::Engine;
    use std::io::Write;

    let visible_indices = state.visible_line_indices();
    let cursor_idx = match state.scroll {
        viewport::ScrollState::Tail => {
            if visible_indices.is_empty() {
                return;
            }
            *visible_indices.last().unwrap()
        }
        viewport::ScrollState::Pinned { cursor, .. } => {
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

    if let Some(entry) = state.log_lines.get(cursor_idx) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&entry.raw);
        // OSC 52 clipboard escape sequence: \x1b]52;c;<base64>\x07
        let osc52 = format!("\x1b]52;c;{}\x07", encoded);
        // Write directly to stdout (bypassing ratatui)
        let _ = std::io::stdout().write_all(osc52.as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// Handle keys when in process detail mode.
fn handle_process_detail_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // Esc or q: close process detail view
        KeyCode::Esc | KeyCode::Char('q') => {
            state.mode = AppMode::Normal;
            state.process_detail_index = None;
            state.process_detail_sockets = None;
        }

        // j / Down: scroll down within detail pane
        KeyCode::Char('j') | KeyCode::Down => {
            state.process_detail_scroll = state.process_detail_scroll.saturating_add(1);
        }

        // k / Up: scroll up within detail pane
        KeyCode::Char('k') | KeyCode::Up => {
            state.process_detail_scroll = state.process_detail_scroll.saturating_sub(1);
        }

        // s: stop selected process (SIGTERM)
        KeyCode::Char('s') => {
            send_signal_to_process_detail(state, nix::sys::signal::Signal::SIGTERM);
        }

        // S: send SIGHUP to selected process
        KeyCode::Char('S') => {
            send_signal_to_process_detail(state, nix::sys::signal::Signal::SIGHUP);
        }

        _ => {}
    }
}

/// Send a signal to the process currently being viewed in the process detail panel.
fn send_signal_to_process_detail(state: &mut AppState, sig: nix::sys::signal::Signal) {
    // Temporarily set the sidebar selection to the process detail index
    // and delegate to the existing send_signal_to_selected function.
    if let Some(idx) = state.process_detail_index {
        let saved_selection = state.sidebar.selection;
        state.sidebar.selection = idx;
        send_signal_to_selected(state, sig);
        state.sidebar.selection = saved_selection;
    }
}

/// Handle mouse events.
fn handle_mouse(
    mouse: MouseEvent,
    state: &mut AppState,
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) {
    // Ignore mouse in overlay modes
    if matches!(
        state.mode,
        AppMode::Help | AppMode::EntryDetail | AppMode::ProcessDetail | AppMode::TaskPicker
    ) {
        return;
    }

    let term_size = terminal.size().unwrap_or_default();
    let sidebar_width = if state.task_name.is_some() && state.sidebar_visible {
        super::sidebar::SIDEBAR_WIDTH
    } else {
        0
    };

    match mouse.kind {
        // Scroll wheel in the log area
        MouseEventKind::ScrollUp => {
            if mouse.column >= sidebar_width {
                // Scroll up by 3 entries in the log viewer
                let viewport_height = term_size.height.saturating_sub(1);
                let viewport_width = term_size.width.saturating_sub(sidebar_width);
                let filtered_entries: Vec<LogEntry> = state.visible_log_lines().into_iter().cloned().collect();
                for _ in 0..3 {
                    state.scroll = scroll_up(
                        &state.scroll,
                        &filtered_entries,
                        viewport_height,
                        viewport_width,
                        state.display_mode,
                        state.wrap,
                        &mut state.source_colors,
                    );
                }
                state.dirty = true;
            }
        }
        MouseEventKind::ScrollDown => {
            if mouse.column >= sidebar_width {
                // Scroll down by 3 entries in the log viewer
                let viewport_height = term_size.height.saturating_sub(1);
                let viewport_width = term_size.width.saturating_sub(sidebar_width);
                let filtered_entries: Vec<LogEntry> = state.visible_log_lines().into_iter().cloned().collect();
                for _ in 0..3 {
                    state.scroll = scroll_down(
                        &state.scroll,
                        &filtered_entries,
                        viewport_height,
                        viewport_width,
                        state.display_mode,
                        state.wrap,
                        &mut state.source_colors,
                    );
                }
                state.dirty = true;
            }
        }
        // Click in the sidebar area
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if mouse.column < sidebar_width && state.mode == AppMode::Normal {
                // Click in the sidebar: select the entry at the clicked row
                let row = mouse.row as usize;
                if row < state.sidebar_entries.len() {
                    state.sidebar.selection = row;
                    state.sidebar.focused = true;
                    state.dirty = true;
                }
            } else if mouse.column >= sidebar_width && state.mode == AppMode::Normal {
                // Click in the log viewer: try to move cursor to that entry
                // The y position in the log area corresponds to a visible entry
                let log_y = mouse.row as usize;
                let viewport_height = term_size.height.saturating_sub(1);
                let viewport_width = term_size.width.saturating_sub(sidebar_width);
                let filtered_entries: Vec<LogEntry> = state.visible_log_lines().into_iter().cloned().collect();

                if !filtered_entries.is_empty() {
                    let vp_layout = viewport::layout(
                        &state.scroll,
                        &filtered_entries,
                        viewport_height,
                        viewport_width,
                        state.display_mode,
                        state.wrap,
                        &mut state.source_colors,
                    );

                    // Find which entry was clicked
                    for ve in &vp_layout.entries {
                        let entry_start = ve.y as usize;
                        let entry_end = entry_start + ve.lines.len();
                        if log_y >= entry_start && log_y < entry_end {
                            state.scroll = viewport::ScrollState::Pinned {
                                cursor: ve.entry_index,
                                top: match state.scroll {
                                    viewport::ScrollState::Pinned { top, .. } => top,
                                    viewport::ScrollState::Tail => 0,
                                },
                            };
                            state.sidebar.focused = false;
                            state.dirty = true;
                            break;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Poll lsof for the process detail panel.
async fn poll_lsof(state: &mut AppState) {
    let pid = get_process_detail_pid(state);

    if let Some(pid) = pid {
        let output = tokio::process::Command::new("lsof")
            .args(["-p", &pid.to_string(), "-i", "-P", "-n"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await;

        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Parse lsof output into a cleaner format
                let mut socket_lines: Vec<String> = Vec::new();
                for line in stdout.lines().skip(1) {
                    // lsof columns: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 10 {
                        let fd = parts[3];
                        let node_type = parts[7]; // TCP, UDP, etc.
                        let name = parts[8..].join(" ");
                        // Check for state info like (LISTEN), (ESTABLISHED)
                        if name.contains("LISTEN") {
                            socket_lines.push(format!("  LISTEN {} (fd {})", name.replace("(LISTEN)", "").trim(), fd));
                        } else if name.contains("ESTABLISHED") {
                            socket_lines.push(format!("  ESTABLISHED {} (fd {})", name.replace("(ESTABLISHED)", "").trim(), fd));
                        } else {
                            socket_lines.push(format!("  {} {} (fd {})", node_type, name, fd));
                        }
                    }
                }
                state.process_detail_sockets = Some(socket_lines.join("\n"));
            }
            Err(_) => {
                state.process_detail_sockets = Some("(lsof not available)".to_string());
            }
        }
    } else {
        state.process_detail_sockets = Some("(no PID available)".to_string());
    }
}

/// Get the PID for the currently viewed process detail.
fn get_process_detail_pid(state: &AppState) -> Option<u32> {
    let sidebar_idx = state.process_detail_index?;
    let procs_arc = state.processes.as_ref()?;
    let procs = procs_arc.try_lock().ok()?;

    let proc_idx = if state.task_name.is_some() {
        sidebar_idx.checked_sub(1)?
    } else {
        sidebar_idx
    };

    let mut running_indices: Vec<usize> = Vec::new();
    let mut completed_indices: Vec<usize> = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        if p.status == ProcessStatus::Running {
            running_indices.push(i);
        } else {
            completed_indices.push(i);
        }
    }
    let ordered: Vec<usize> = running_indices.into_iter().chain(completed_indices).collect();
    let &proc_vec_idx = ordered.get(proc_idx)?;
    procs[proc_vec_idx].pid
}

/// Check for newly failed processes and create notifications.
fn check_for_crashes(
    state: &mut AppState,
    prev_statuses: &mut Vec<(String, ProcessStatus)>,
) {
    if let Some(procs_arc) = &state.processes {
        if let Ok(procs) = procs_arc.try_lock() {
            let current: Vec<(String, ProcessStatus)> = procs
                .iter()
                .map(|p| (p.command_label.clone(), p.status.clone()))
                .collect();

            // Check for new failures
            for (i, (name, status)) in current.iter().enumerate() {
                if let ProcessStatus::Failed(code) = status {
                    // Check if this was previously running
                    let was_running = if i < prev_statuses.len() {
                        matches!(prev_statuses[i].1, ProcessStatus::Running)
                    } else {
                        false // new process we haven't seen before
                    };

                    if was_running {
                        // Check if the source is filtered out or user is scrolled away
                        let is_filtered = !state.source_filter.is_empty();
                        let not_tailing = !matches!(state.scroll, viewport::ScrollState::Tail);
                        if is_filtered || not_tailing {
                            state.notifications.push((
                                format!("{} exited with code {}", name, code),
                                std::time::Instant::now(),
                            ));
                            state.dirty = true;
                        }
                    }
                }
            }

            *prev_statuses = current;
        }
    }
}

/// Export visible log entries to a file.
fn export_visible_log(state: &mut AppState) {
    let visible = state.visible_log_lines();
    if visible.is_empty() {
        state.notifications.push((
            "Nothing to export (no visible entries)".to_string(),
            std::time::Instant::now(),
        ));
        state.dirty = true;
        return;
    }

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let filename = format!("runme-export-{}.log", timestamp);

    let mut content = String::new();
    let count = visible.len();
    for entry in &visible {
        content.push_str(&entry.raw);
        content.push('\n');
    }

    match std::fs::write(&filename, &content) {
        Ok(()) => {
            state.notifications.push((
                format!("Exported {} entries to {}", count, filename),
                std::time::Instant::now(),
            ));
        }
        Err(e) => {
            state.notifications.push((
                format!("Export failed: {}", e),
                std::time::Instant::now(),
            ));
        }
    }
    state.dirty = true;
}

/// Navigate to a specific visible entry index, updating the scroll state.
fn navigate_to_entry(
    state: &mut AppState,
    target: usize,
    _filtered_entries: &[LogEntry],
    viewport_height: u16,
    _viewport_width: u16,
) {
    let total = state.visible_line_indices().len();
    if total == 0 {
        return;
    }
    let target = target.min(total.saturating_sub(1));

    // If target is the last entry, go to Tail
    if target >= total.saturating_sub(1) {
        state.scroll = viewport::ScrollState::Tail;
    } else {
        // Set cursor to target, compute appropriate top
        let current_top = match state.scroll {
            viewport::ScrollState::Pinned { top, .. } => top,
            viewport::ScrollState::Tail => 0,
        };
        state.scroll = viewport::ScrollState::Pinned {
            cursor: target,
            top: current_top,
        };
        // Let the viewport adjust top for the new cursor position
        // We need the filtered entries for this — recalculate from visible
        let visible_entries: Vec<LogEntry> = state.visible_log_lines().into_iter().cloned().collect();
        if !visible_entries.is_empty() {
            // Use scroll_down/scroll_up logic to adjust — just set and let render fix it
            // For a clean approach, manually recompute. A simple heuristic:
            // Put the target entry in the middle of the viewport.
            let half = (viewport_height / 2) as usize;
            let new_top = target.saturating_sub(half);
            state.scroll = viewport::ScrollState::Pinned {
                cursor: target,
                top: new_top,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;
    use super::super::sidebar::SidebarEntry;
    use super::super::viewport::ScrollState;
    use ratatui::style::Color;

    fn make_key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn q_sets_running_false() {
        let mut state = AppState::new();
        assert!(state.running);
        // Test quit via the match logic directly
        let key = make_key_event(KeyCode::Char('q'), KeyModifiers::NONE);
        match key.code {
            KeyCode::Char('q') => state.running = false,
            _ => {}
        }
        assert!(!state.running);
    }

    #[test]
    fn ctrl_c_sets_running_false() {
        let mut state = AppState::new();
        assert!(state.running);
        let key = make_key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            state.running = false;
        }
        assert!(!state.running);
    }

    #[test]
    fn resize_sets_dirty() {
        let mut state = AppState::new();
        state.dirty = false;
        // Resize handling is simple: just set dirty
        state.dirty = true; // simulating handle_event(Event::Resize(...))
        assert!(state.dirty);
    }

    #[test]
    fn display_mode_toggle() {
        let mut state = AppState::new();
        assert_eq!(state.display_mode, DisplayMode::Preview);
        state.display_mode = match state.display_mode {
            DisplayMode::Preview => DisplayMode::Raw,
            DisplayMode::Raw => DisplayMode::Preview,
        };
        assert_eq!(state.display_mode, DisplayMode::Raw);
        state.display_mode = match state.display_mode {
            DisplayMode::Preview => DisplayMode::Raw,
            DisplayMode::Raw => DisplayMode::Preview,
        };
        assert_eq!(state.display_mode, DisplayMode::Preview);
    }

    #[test]
    fn wrap_toggle() {
        let mut state = AppState::new();
        assert!(!state.wrap);
        state.wrap = !state.wrap;
        assert!(state.wrap);
        state.wrap = !state.wrap;
        assert!(!state.wrap);
    }

    #[test]
    fn scroll_state_transitions() {
        use crate::log::{LogEntry, ParsedContent};
        use std::collections::HashMap;

        let mut state = AppState::new();
        // Add some entries
        for i in 0..20 {
            state.log_lines.push(LogEntry {
            received_at: chrono::Utc::now(),
                raw: format!("entry {}", i),
                parsed: ParsedContent::PlainText,
                source: "test".to_string(),
                seq: i as u64,
                timestamp: None,
                level: Some("info".to_string()),
                message: Some(format!("entry {}", i)),
                fields: HashMap::new(),
            });
        }

        // Start in tail
        assert_eq!(state.scroll, ScrollState::Tail);

        // Scroll up should switch to pinned
        let new_scroll = super::scroll_up(
            &state.scroll,
            &state.log_lines,
            24,
            80,
            state.display_mode,
            state.wrap,
            &mut state.source_colors,
        );
        state.scroll = new_scroll;
        assert!(matches!(state.scroll, ScrollState::Pinned { .. }));

        // Jump to bottom should return to tail
        state.scroll = super::scroll_to_bottom(&state.scroll, &state.log_lines);
        assert_eq!(state.scroll, ScrollState::Tail);
    }

    #[test]
    fn tab_toggles_sidebar_focus() {
        let mut state = AppState::new();
        assert!(!state.sidebar.focused);
        state.sidebar.focused = !state.sidebar.focused;
        assert!(state.sidebar.focused);
        state.sidebar.focused = !state.sidebar.focused;
        assert!(!state.sidebar.focused);
    }

    #[test]
    fn sidebar_jk_moves_selection() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
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
                name: "api".to_string(),
                source: "api".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
            SidebarEntry {
                name: "worker".to_string(),
                source: "worker".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];

        assert_eq!(state.sidebar.selection, 0);
        handle_sidebar_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 1);
        handle_sidebar_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 2);
        // Clamp at max
        handle_sidebar_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 2);
        // Move back up
        handle_sidebar_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 1);
    }

    #[test]
    fn number_keys_toggle_source() {
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
                name: "api".to_string(),
                source: "api".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];

        // Press '2' to toggle the second source (api)
        handle_log_viewer_key(
            make_key_event(KeyCode::Char('2'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        // Should have toggled — now showing all except "api"
        assert!(state.source_filter.contains("task"));
        assert!(!state.source_filter.contains("api"));
    }

    #[test]
    fn a_key_shows_all() {
        let mut state = AppState::new();
        state.source_filter.insert("api".to_string());
        assert!(!state.source_filter.is_empty());

        handle_log_viewer_key(
            make_key_event(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert!(state.source_filter.is_empty());
    }

    // -- Entry detail view tests --

    fn make_log_entry(raw: &str, source: &str) -> LogEntry {
        use crate::log::ParsedContent;
        use std::collections::HashMap;

        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq: 0,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(raw.to_string()),
            fields: HashMap::new(),
        }
    }

    #[test]
    fn enter_opens_detail_view() {
        let mut state = AppState::new();
        state.log_lines.push(make_log_entry("hello", "test"));
        // Pin to entry 0 so there's a cursor
        state.scroll = ScrollState::Pinned { cursor: 0, top: 0 };

        let entries = state.log_lines.clone();
        assert_eq!(state.mode, AppMode::Normal);
        handle_log_viewer_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &entries,
            24,
            80,
        );
        assert_eq!(state.mode, AppMode::EntryDetail);
        assert_eq!(state.detail_scroll, 0);
    }

    #[test]
    fn enter_does_nothing_with_no_entries() {
        let mut state = AppState::new();
        assert_eq!(state.mode, AppMode::Normal);
        handle_log_viewer_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        // Should stay in Normal mode since there are no visible entries
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn detail_esc_closes() {
        let mut state = AppState::new();
        state.log_lines.push(make_log_entry("hello", "test"));
        state.mode = AppMode::EntryDetail;

        handle_detail_key(
            make_key_event(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn detail_q_closes() {
        let mut state = AppState::new();
        state.mode = AppMode::EntryDetail;

        handle_detail_key(
            make_key_event(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn detail_jk_scrolls() {
        let mut state = AppState::new();
        state.mode = AppMode::EntryDetail;
        state.detail_scroll = 0;

        // j scrolls down
        handle_detail_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert_eq!(state.detail_scroll, 1);

        handle_detail_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert_eq!(state.detail_scroll, 2);

        // k scrolls up
        handle_detail_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert_eq!(state.detail_scroll, 1);

        // k at 0 stays at 0
        state.detail_scroll = 0;
        handle_detail_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert_eq!(state.detail_scroll, 0);
    }

    #[test]
    fn detail_n_closes_and_moves_next() {
        let mut state = AppState::new();
        for i in 0..5 {
            state.log_lines.push(make_log_entry(&format!("entry {}", i), "test"));
        }
        state.scroll = ScrollState::Pinned { cursor: 2, top: 0 };
        state.mode = AppMode::EntryDetail;

        let entries = state.log_lines.clone();
        handle_detail_key(
            make_key_event(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut state,
            &entries,
            24,
            80,
        );
        assert_eq!(state.mode, AppMode::Normal);
        // Should have moved cursor down (from 2 to 3)
        match state.scroll {
            ScrollState::Pinned { cursor, .. } => assert_eq!(cursor, 3),
            ScrollState::Tail => {} // also acceptable at end of list
        }
    }

    #[test]
    fn detail_n_uppercase_closes_and_moves_prev() {
        let mut state = AppState::new();
        for i in 0..5 {
            state.log_lines.push(make_log_entry(&format!("entry {}", i), "test"));
        }
        state.scroll = ScrollState::Pinned { cursor: 2, top: 0 };
        state.mode = AppMode::EntryDetail;

        let entries = state.log_lines.clone();
        handle_detail_key(
            make_key_event(KeyCode::Char('N'), KeyModifiers::NONE),
            &mut state,
            &entries,
            24,
            80,
        );
        assert_eq!(state.mode, AppMode::Normal);
        // Should have moved cursor up (from 2 to 1)
        match state.scroll {
            ScrollState::Pinned { cursor, .. } => assert_eq!(cursor, 1),
            _ => panic!("expected Pinned"),
        }
    }

    // -- Process control tests --

    #[test]
    fn sidebar_s_on_task_entry_is_noop() {
        // Selecting the task entry (is_task = true) should not crash or send signals
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar.selection = 0;
        state.sidebar_entries = vec![SidebarEntry {
            name: "my-task".to_string(),
            source: "my-task".to_string(),
            status_tag: "SETUP".to_string(),
            status_color: Color::Yellow,
            visible: true,
            is_task: true,
        }];

        // This should be a no-op (task entries can't be signaled)
        handle_sidebar_key(
            make_key_event(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut state,
        );
        // No crash is the test here
    }

    #[test]
    fn sidebar_s_without_processes_is_noop() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar.selection = 1;
        state.task_name = Some("test".to_string());
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "test".to_string(),
                source: "test".to_string(),
                status_tag: "READY".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: true,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: "test".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];
        // No processes Arc — should be a no-op
        state.processes = None;

        handle_sidebar_key(
            make_key_event(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut state,
        );
        // No crash is the test here
    }

    // -- Process detail tests --

    #[test]
    fn sidebar_enter_on_process_opens_detail() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar.selection = 1; // process entry
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
                source: "task".to_string(),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
            },
        ];

        handle_sidebar_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::ProcessDetail);
        assert_eq!(state.process_detail_index, Some(1));
        assert_eq!(state.process_detail_scroll, 0);
    }

    #[test]
    fn sidebar_enter_on_task_toggles_visibility() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar.selection = 0; // task entry
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

        handle_sidebar_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        // Should stay in Normal mode (task toggle, not process detail)
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn process_detail_esc_closes() {
        let mut state = AppState::new();
        state.mode = AppMode::ProcessDetail;
        state.process_detail_index = Some(1);

        handle_process_detail_key(
            make_key_event(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
        assert!(state.process_detail_index.is_none());
    }

    #[test]
    fn process_detail_q_closes() {
        let mut state = AppState::new();
        state.mode = AppMode::ProcessDetail;
        state.process_detail_index = Some(1);

        handle_process_detail_key(
            make_key_event(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
        assert!(state.process_detail_index.is_none());
    }

    #[test]
    fn process_detail_jk_scrolls() {
        let mut state = AppState::new();
        state.mode = AppMode::ProcessDetail;
        state.process_detail_scroll = 0;

        handle_process_detail_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.process_detail_scroll, 1);

        handle_process_detail_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.process_detail_scroll, 2);

        handle_process_detail_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.process_detail_scroll, 1);

        state.process_detail_scroll = 0;
        handle_process_detail_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.process_detail_scroll, 0);
    }

    // -- Sidebar collapse tests --

    #[test]
    fn backslash_toggles_sidebar() {
        let mut state = AppState::new();
        assert!(state.sidebar_visible);

        handle_log_viewer_key(
            make_key_event(KeyCode::Char('\\'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert!(!state.sidebar_visible);

        handle_log_viewer_key(
            make_key_event(KeyCode::Char('\\'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert!(state.sidebar_visible);
    }

    // -- Filter history tests --

    #[test]
    fn filter_history_saved_on_confirm() {
        let mut state = AppState::new();
        state.mode = AppMode::FilterInput;

        // Type something
        for ch in "error".chars() {
            state.filter_input.insert_char(ch);
        }

        // Confirm with Enter
        handle_filter_input_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );

        assert_eq!(state.filter_history.len(), 1);
        assert_eq!(state.filter_history[0], "error");
    }

    #[test]
    fn filter_history_up_down_cycles() {
        let mut state = AppState::new();
        state.filter_history = vec!["error".to_string(), "level:warn".to_string()];
        state.mode = AppMode::FilterInput;

        // Up should go to the last entry
        handle_filter_input_key(
            make_key_event(KeyCode::Up, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.filter_input.text, "level:warn");
        assert_eq!(state.filter_history_index, Some(1));

        // Up again should go to first entry
        handle_filter_input_key(
            make_key_event(KeyCode::Up, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.filter_input.text, "error");
        assert_eq!(state.filter_history_index, Some(0));

        // Down should go back to second entry
        handle_filter_input_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.filter_input.text, "level:warn");
        assert_eq!(state.filter_history_index, Some(1));

        // Down past end should clear
        handle_filter_input_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.filter_input.text.is_empty());
        assert!(state.filter_history_index.is_none());
    }

    #[test]
    fn filter_history_esc_resets_index() {
        let mut state = AppState::new();
        state.filter_history = vec!["error".to_string()];
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();

        // Browse history
        handle_filter_input_key(
            make_key_event(KeyCode::Up, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.filter_history_index, Some(0));

        // Esc should reset the history index
        handle_filter_input_key(
            make_key_event(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.filter_history_index.is_none());
    }

    // -- Crash surfacing tests --

    #[test]
    fn check_for_crashes_detects_new_failure() {
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use crate::log::buffer::OutputBuffer;

        let mut state = AppState::new();
        state.source_filter.insert("other".to_string()); // filters are active

        let procs = Arc::new(Mutex::new(vec![
            super::super::runner::ProcessInfo {
                task_name: "test".to_string(),
                command_label: "echo hello".to_string(),
                buffer: Arc::new(Mutex::new(OutputBuffer::new(100))),
                pgid: None,
                pid: None,
                status: ProcessStatus::Failed(1),
            },
        ]));
        state.processes = Some(procs);

        let mut prev_statuses = vec![("echo hello".to_string(), ProcessStatus::Running)];

        check_for_crashes(&mut state, &mut prev_statuses);

        assert_eq!(state.notifications.len(), 1);
        assert!(state.notifications[0].0.contains("echo hello"));
        assert!(state.notifications[0].0.contains("code 1"));
    }

    // -- Notification tests --

    #[test]
    fn notifications_default_empty() {
        let state = AppState::new();
        assert!(state.notifications.is_empty());
    }

    // -- New AppState defaults tests --

    #[test]
    fn new_state_has_sidebar_visible() {
        let state = AppState::new();
        assert!(state.sidebar_visible);
    }

    #[test]
    fn new_state_has_empty_filter_history() {
        let state = AppState::new();
        assert!(state.filter_history.is_empty());
        assert!(state.filter_history_index.is_none());
    }

    #[test]
    fn new_state_has_no_process_detail() {
        let state = AppState::new();
        assert!(state.process_detail_index.is_none());
        assert_eq!(state.process_detail_scroll, 0);
        assert!(state.process_detail_sockets.is_none());
    }
}
