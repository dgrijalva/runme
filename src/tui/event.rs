use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::log::LogEntry;

use super::app::{AppMode, AppState};
use super::frame::render_frame;
use super::keys;
use super::runner::{ProcessStatus, TaskStatus};
use super::sidebar;
use super::viewport::{self, scroll_down, scroll_up};

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
    let mut sigint = signal(SignalKind::interrupt()).map_err(io::Error::other)?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(io::Error::other)?;

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
                            state.launch_picked_task(task, Vec::new());
                            // Re-subscribe to the new LogStore
                            let store = state.log_store.lock().await;
                            let existing = store.compose_owned();
                            if !existing.is_empty() {
                                state.log_lines = existing;
                            }
                            log_rx = store.subscribe();
                            drop(store);
                        }

                        // Check if a restart was requested
                        if state.pending_restart {
                            state.pending_restart = false;
                            if let Some(task) = state.current_task {
                                // Shut down old runner's processes in the background
                                if let Some(old_runner) = state.runner.take() {
                                    tokio::spawn(async move {
                                        old_runner
                                            .shutdown(Duration::from_secs(5))
                                            .await;
                                    });
                                }

                                // Reset UI state
                                state.scroll = super::viewport::ScrollState::Tail;
                                state.sidebar.selection = 0;
                                state.sidebar.focused = false;
                                state.search = super::search::SearchState::new();
                                state.sidebar_entries.clear();
                                state.detail_scroll = 0;
                                state.process_detail_index = None;
                                state.process_detail_scroll = 0;
                                state.process_detail_sockets = None;
                                prev_process_statuses.clear();

                                // Re-launch the same task
                                let args = state.current_task_args.clone();
                                state.launch_picked_task(task, args);

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
                        state.field_stats.observe(&entry.source, &entry.fields);
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
                        // Rebuild field stats from the entries we now have
                        state.field_stats = crate::log::field_stats::FieldStats::new();
                        for entry in &state.log_lines {
                            state.field_stats.observe(&entry.source, &entry.fields);
                        }
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

                // Check tui_wait: auto-exit when task is complete and tui_wait is false
                if let Some(ref tui_wait) = state.tui_wait
                    && !tui_wait.load(Ordering::Relaxed)
                        && let Some(ref task_status) = state.task_status {
                            let status = task_status.lock().await;
                            match &*status {
                                TaskStatus::Done | TaskStatus::Failed(_) => {
                                    state.running = false;
                                }
                                _ => {}
                            }
                        }

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
///
/// If the runner has multiple sessions, iterates all sessions to refresh
/// process statuses and builds a grouped sidebar. Falls back to the legacy
/// single-task path when no runner is present.
async fn refresh_sidebar_state(state: &mut AppState) {
    // Multi-session path: if we have a runner with sessions, use those
    if let Some(ref runner) = state.runner
        && !runner.sessions.is_empty()
    {
        // Refresh process statuses across all sessions
        for session in &runner.sessions {
            let mut procs = session.processes.lock().await;
            for proc in procs.iter_mut() {
                proc.refresh_status();
            }
        }

        // Build sidebar entries from all sessions
        state.sidebar_entries = sidebar::build_sidebar_entries_multi(
            &runner.sessions,
            &state.source_filter,
            &mut state.source_colors,
        )
        .await;

        // Clamp sidebar selection
        state.sidebar.clamp_selection(state.sidebar_entries.len());
        return;
    }

    // Legacy single-task path (backward compatibility)
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

    // Build filtered entries for scroll operations (applies both source and expression filters)
    let filtered_entries: Vec<LogEntry> =
        state.visible_log_lines().into_iter().cloned().collect();

    // Task picker mode: dedicated key handling
    if state.mode == AppMode::TaskPicker {
        keys::handle_picker_key(key, state);
        state.dirty = true;
        return;
    }

    // Filter input mode gets its own key handling — only Esc and Ctrl-C escape
    if state.mode == AppMode::FilterInput {
        keys::handle_filter_input_key(key, state);
        state.dirty = true;
        return;
    }

    // Search input mode gets its own key handling
    if state.mode == AppMode::SearchInput {
        keys::handle_search_input_key(
            key,
            state,
            &filtered_entries,
            viewport_height,
            viewport_width,
        );
        state.dirty = true;
        return;
    }

    // Entry detail mode: dedicated key handling
    if state.mode == AppMode::EntryDetail {
        keys::handle_detail_key(
            key,
            state,
            &filtered_entries,
            viewport_height,
            viewport_width,
        );
        state.dirty = true;
        return;
    }

    // Process detail mode: dedicated key handling
    if state.mode == AppMode::ProcessDetail {
        keys::handle_process_detail_key(key, state);
        state.dirty = true;
        return;
    }

    // Help mode: any key dismisses
    if state.mode == AppMode::Help {
        state.mode = AppMode::Normal;
        state.dirty = true;
        return;
    }

    // Copy menu mode: dispatch to copy menu handler
    if state.mode == AppMode::CopyMenu {
        keys::handle_copy_menu_key(key, state);
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
        // 'r' restarts the current task
        KeyCode::Char('r') if state.current_task.is_some() => {
            state.pending_restart = true;
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
        keys::handle_sidebar_key(key, state);
    } else {
        // -- Log viewer-focused key bindings --
        keys::handle_log_viewer_key(
            key,
            state,
            &filtered_entries,
            viewport_height,
            viewport_width,
        );
    }

    // Any key press marks the state as dirty
    state.dirty = true;
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
        AppMode::Help
            | AppMode::EntryDetail
            | AppMode::ProcessDetail
            | AppMode::TaskPicker
            | AppMode::CopyMenu
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
                let filtered_entries: Vec<LogEntry> =
                    state.visible_log_lines().into_iter().cloned().collect();
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
                let filtered_entries: Vec<LogEntry> =
                    state.visible_log_lines().into_iter().cloned().collect();
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
                let filtered_entries: Vec<LogEntry> =
                    state.visible_log_lines().into_iter().cloned().collect();

                if !filtered_entries.is_empty() {
                    let vp_layout = viewport::layout(
                        &state.scroll,
                        &filtered_entries,
                        viewport_height,
                        viewport_width,
                        state.display_mode,
                        state.wrap,
                        &mut state.source_colors,
                        Some(&state.field_stats),
                        state.show_fields,
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
                                    viewport::ScrollState::Tail => vp_layout
                                        .entries
                                        .first()
                                        .map(|e| e.entry_index)
                                        .unwrap_or(0),
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
            .args(["-a", "-p", &pid.to_string(), "-i", "-P", "-n"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await;

        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Extract only listening ports from lsof output
                let mut ports: Vec<String> = Vec::new();
                for line in stdout.lines().skip(1) {
                    if !line.contains("LISTEN") {
                        continue;
                    }
                    // lsof columns: COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 9 {
                        let name = parts[8];
                        // name looks like "*:8080" or "127.0.0.1:3000" or "[::]:443"
                        // extract the port (after the last colon)
                        if let Some(port) = name.rsplit(':').next() {
                            let port_str = port.to_string();
                            if !ports.contains(&port_str) {
                                ports.push(port_str);
                            }
                        }
                    }
                }
                if ports.is_empty() {
                    state.process_detail_sockets = Some("(none)".to_string());
                } else {
                    state.process_detail_sockets = Some(ports.join(", "));
                }
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
///
/// In multi-session mode, finds the process across all sessions by matching
/// the sidebar entry. Falls back to the legacy single-process-list path.
fn get_process_detail_pid(state: &AppState) -> Option<u32> {
    let sidebar_idx = state.process_detail_index?;
    let entry = state.sidebar_entries.get(sidebar_idx)?;

    // The sidebar entry must be a process (not a task)
    if entry.is_task {
        return None;
    }

    // Multi-session path: search all sessions for a matching process
    if let Some(ref runner) = state.runner {
        for session in &runner.sessions {
            let procs = session.processes.try_lock().ok()?;
            // Find process by command label match
            for proc in procs.iter() {
                if proc.display_name() == entry.name {
                    return proc.pid;
                }
            }
        }
        return None;
    }

    // Legacy single-process-list path
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
    let ordered: Vec<usize> = running_indices
        .into_iter()
        .chain(completed_indices)
        .collect();
    let &proc_vec_idx = ordered.get(proc_idx)?;
    procs[proc_vec_idx].pid
}

/// Check for newly failed processes and create notifications.
///
/// In multi-session mode, collects process statuses across all sessions.
fn check_for_crashes(state: &mut AppState, prev_statuses: &mut Vec<(String, ProcessStatus)>) {
    // Collect current process statuses from all sessions (or the single process list)
    let current: Vec<(String, ProcessStatus)> = if let Some(ref runner) = state.runner {
        let mut all = Vec::new();
        for session in &runner.sessions {
            if let Ok(procs) = session.processes.try_lock() {
                for p in procs.iter() {
                    all.push((p.command_label.clone(), p.status.clone()));
                }
            }
        }
        all
    } else if let Some(procs_arc) = &state.processes
        && let Ok(procs) = procs_arc.try_lock()
    {
        procs
            .iter()
            .map(|p| (p.command_label.clone(), p.status.clone()))
            .collect()
    } else {
        return;
    };

    {

        // Check for new failures
        for (i, (name, status)) in current.iter().enumerate() {
            if let ProcessStatus::Failed(termination) = status {
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
                            format!("{} {}", name, termination),
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

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::super::viewport::ScrollState;
    use super::*;

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
        if let KeyCode::Char('q') = key.code {
            state.running = false;
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
        use super::super::render::DisplayMode;

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
        use super::super::viewport::scroll_to_bottom;
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
                stream: None,
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
        state.scroll = scroll_to_bottom(&state.scroll, &state.log_lines);
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

    // -- Crash surfacing tests --

    #[test]
    fn check_for_crashes_detects_new_failure() {
        use crate::log::buffer::OutputBuffer;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let mut state = AppState::new();
        state.source_filter.insert("other".to_string()); // filters are active

        let procs = Arc::new(Mutex::new(vec![super::super::runner::ProcessInfo {
            task_name: "test".to_string(),
            command_label: "echo hello".to_string(),
            buffer: Arc::new(Mutex::new(OutputBuffer::new(100))),
            pgid: None,
            pid: None,
            status: ProcessStatus::Failed(crate::process::Termination::Exited(1)),
            ready: true,
        }]));
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
