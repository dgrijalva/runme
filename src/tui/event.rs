use std::io;
use std::time::Duration;

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::execution::TaskId;
use crate::log::LogEntry;

use super::app::{AppMode, AppState};
use super::frame::render_frame;
use super::keys;
use super::runner::ProcessStatus;
use super::sidebar;
use super::viewport::{self, scroll_down, scroll_up};

/// Target frame interval (~60fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Run the main event loop. Drives the TUI:
///
/// - Polls terminal events (keyboard, mouse, resize)
/// - Receives log entries from the engine's `LogStore` broadcast
/// - Re-renders when the dirty flag is set
/// - Quits cleanly on SIGINT/SIGTERM (engine teardown is the caller's job)
pub async fn run_event_loop(
    state: &mut AppState,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<()> {
    let mut event_stream = EventStream::new();
    let mut render_interval = tokio::time::interval(FRAME_INTERVAL);
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigint = signal(SignalKind::interrupt()).map_err(io::Error::other)?;
    let mut sigterm = signal(SignalKind::terminate()).map_err(io::Error::other)?;

    // Load any entries already in the LogStore and subscribe for new ones.
    let mut log_rx: broadcast::Receiver<LogEntry> = {
        let store = state.log_store.lock().await;
        let existing = store.compose_owned();
        if !existing.is_empty() {
            state.log_lines = existing;
            state.dirty = true;
        }
        store.subscribe()
    };

    // Watch the engine's graph for changes — every snapshot publish wakes
    // the event loop so the sidebar/status redraws immediately rather than
    // only on the next 60fps tick. Held as Option to handle the (rare,
    // tests-only) case where no engine is wired.
    let mut graph_rx = state.engine.as_ref().map(|e| e.graph.clone());

    // Timer for lsof polling and notification cleanup.
    let mut lsof_interval = tokio::time::interval(Duration::from_secs(3));
    lsof_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    lsof_interval.tick().await;

    // Track previous process statuses (keyed by process TaskId) for crash surfacing.
    let mut prev_process_statuses: Vec<(TaskId, ProcessStatus)> = Vec::new();

    while state.running {
        // Build a future that resolves when either the graph changes or
        // never (when no engine is wired).
        let graph_changed = async {
            match graph_rx.as_mut() {
                Some(rx) => {
                    let _ = rx.changed().await;
                }
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if matches!(event, Event::Key(_)) {
                            state.notifications.clear();
                        }
                        handle_event(event, state, terminal);

                        if let Some(task) = state.pending_task.take() {
                            state.launch_picked_task(task, Vec::new()).await;
                        }

                        if state.pending_restart {
                            state.pending_restart = false;
                            if let Some(task) = state.current_task {
                                if let (Some(handle), Some(prev_id)) =
                                    (state.engine.clone(), state.current_task_id.take())
                                {
                                    tokio::spawn(async move {
                                        let _ = handle
                                            .kill_task(prev_id, crate::execution::KillSignal::Term)
                                            .await;
                                    });
                                }

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

                                let args = state.current_task_args.clone();
                                state.launch_picked_task(task, args).await;
                            }
                        }
                    }
                    Some(Err(_)) => {
                        state.running = false;
                    }
                    None => {
                        state.running = false;
                    }
                }
            }

            result = log_rx.recv() => {
                match result {
                    Ok(entry) => {
                        if state.search.active {
                            let visible_count = state.visible_line_indices().len();
                            let text = entry.message.as_deref().unwrap_or(&entry.raw);
                            state.search.check_new_entry(visible_count, text);
                        }
                        state.field_stats.observe(entry.source, &entry.fields);
                        state.log_lines.push(entry);
                        state.dirty = true;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let store = state.log_store.lock().await;
                        state.log_lines = store.compose_owned();
                        state.field_stats = crate::log::field_stats::FieldStats::new();
                        for entry in &state.log_lines {
                            state.field_stats.observe(entry.source, &entry.fields);
                        }
                        state.dirty = true;
                        drop(store);
                        let _ = n;
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
                        // Log stream closed; keep running.
                    }
                }
            }

            _ = graph_changed => {
                state.dirty = true;
            }

            _ = render_interval.tick(), if state.dirty => {
                refresh_sidebar_state(state).await;
                check_for_crashes(state, &mut prev_process_statuses);

                let now = std::time::Instant::now();
                state
                    .notifications
                    .retain(|(_, ts)| now.duration_since(*ts) < Duration::from_secs(5));

                render_frame(terminal, state)?;
                state.dirty = false;
            }

            _ = lsof_interval.tick(), if state.mode == AppMode::ProcessDetail => {
                poll_lsof(state).await;
                state.dirty = true;
            }

            _ = sigint.recv() => {
                state.running = false;
            }

            _ = sigterm.recv() => {
                state.running = false;
            }
        }
    }

    Ok(())
}

/// Rebuild the sidebar entries from the engine's `GraphSnapshot`.
async fn refresh_sidebar_state(state: &mut AppState) {
    let Some(handle) = state.engine.as_ref() else {
        state.sidebar_entries.clear();
        return;
    };
    let snapshot = handle.graph.borrow().clone();
    state.sidebar_entries = sidebar::build_sidebar_entries_from_graph(
        &snapshot,
        &state.source_filter,
        &mut state.source_colors,
    );
    state.sidebar.clamp_selection(state.sidebar_entries.len());
}

/// Detect newly-failed processes from the latest graph snapshot and post a
/// crash notification when the failure is hidden by the active filter or
/// the user has scrolled away from the tail.
fn check_for_crashes(
    state: &mut AppState,
    prev_statuses: &mut Vec<(TaskId, ProcessStatus)>,
) {
    let Some(handle) = state.engine.as_ref() else {
        return;
    };
    let snapshot = handle.graph.borrow().clone();
    let mut current: Vec<(TaskId, ProcessStatus, String)> = Vec::new();
    for node in snapshot.tasks.values() {
        for proc in &node.processes {
            current.push((proc.id, proc.status.clone(), proc.command_label.clone()));
        }
    }

    for (id, status, name) in &current {
        if let ProcessStatus::Failed(termination) = status {
            let was_running = prev_statuses
                .iter()
                .find(|(prev_id, _)| prev_id == id)
                .is_some_and(|(_, s)| matches!(s, ProcessStatus::Running));
            if was_running {
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

    *prev_statuses = current.into_iter().map(|(id, s, _)| (id, s)).collect();
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
            state.dirty = true;
        }
        _ => {}
    }
}

fn handle_key(
    key: KeyEvent,
    state: &mut AppState,
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) {
    let term_size = terminal.size().unwrap_or_default();
    let viewport_height = term_size.height.saturating_sub(1);
    // Use the sidebar entries' presence to decide whether the sidebar is
    // taking up width, rather than a defunct task_name field.
    let sidebar_active = state.sidebar_visible && !state.sidebar_entries.is_empty();
    let sidebar_width = if sidebar_active {
        super::sidebar::SIDEBAR_WIDTH
    } else {
        0
    };
    let viewport_width = term_size.width.saturating_sub(sidebar_width);

    let filtered_entries: Vec<LogEntry> =
        state.visible_log_lines().into_iter().cloned().collect();

    if state.mode == AppMode::TaskPicker {
        keys::handle_picker_key(key, state);
        state.dirty = true;
        return;
    }

    if state.mode == AppMode::FilterInput {
        keys::handle_filter_input_key(key, state);
        state.dirty = true;
        return;
    }

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

    if state.mode == AppMode::ProcessDetail {
        keys::handle_process_detail_key(key, state);
        state.dirty = true;
        return;
    }

    if state.mode == AppMode::Help {
        state.mode = AppMode::Normal;
        state.dirty = true;
        return;
    }

    if state.mode == AppMode::CopyMenu {
        keys::handle_copy_menu_key(key, state);
        state.dirty = true;
        return;
    }

    match key.code {
        KeyCode::Char('?') => {
            state.mode = AppMode::Help;
            state.dirty = true;
            return;
        }
        KeyCode::Char('q') => {
            state.running = false;
            state.dirty = true;
            return;
        }
        KeyCode::Char('r') if state.current_task.is_some() => {
            state.pending_restart = true;
            state.dirty = true;
            return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.running = false;
            state.dirty = true;
            return;
        }
        KeyCode::Tab => {
            state.sidebar.focused = !state.sidebar.focused;
            state.dirty = true;
            return;
        }
        _ => {}
    }

    if state.sidebar.focused {
        keys::handle_sidebar_key(key, state);
    } else {
        keys::handle_log_viewer_key(
            key,
            state,
            &filtered_entries,
            viewport_height,
            viewport_width,
        );
    }

    state.dirty = true;
}

fn handle_mouse(
    mouse: MouseEvent,
    state: &mut AppState,
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) {
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
    let sidebar_active = state.sidebar_visible && !state.sidebar_entries.is_empty();
    let sidebar_width = if sidebar_active {
        super::sidebar::SIDEBAR_WIDTH
    } else {
        0
    };

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if mouse.column >= sidebar_width {
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
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if mouse.column < sidebar_width && state.mode == AppMode::Normal {
                let row = mouse.row as usize;
                if row < state.sidebar_entries.len() {
                    state.sidebar.selection = row;
                    state.sidebar.focused = true;
                    state.dirty = true;
                }
            } else if mouse.column >= sidebar_width && state.mode == AppMode::Normal {
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
                let mut ports: Vec<String> = Vec::new();
                for line in stdout.lines().skip(1) {
                    if !line.contains("LISTEN") {
                        continue;
                    }
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 9 {
                        let name = parts[8];
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

/// Resolve the PID for the process detail panel from the graph snapshot.
fn get_process_detail_pid(state: &AppState) -> Option<u32> {
    let sidebar_idx = state.process_detail_index?;
    let entry = state.sidebar_entries.get(sidebar_idx)?;
    if entry.is_task {
        return None;
    }
    let handle = state.engine.as_ref()?;
    let snapshot = handle.graph.borrow().clone();
    for node in snapshot.tasks.values() {
        for proc in &node.processes {
            if proc.id == entry.source {
                return proc.pid;
            }
        }
    }
    None
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
    fn scroll_state_transitions() {
        use super::super::viewport::scroll_to_bottom;
        use crate::log::{LogEntry, ParsedContent};
        use std::collections::HashMap;

        let mut state = AppState::new();
        for i in 0..20 {
            state.log_lines.push(LogEntry {
                received_at: chrono::Utc::now(),
                raw: format!("entry {}", i),
                parsed: ParsedContent::PlainText,
                source: TaskId(0),
                seq: i as u64,
                timestamp: None,
                level: Some("info".to_string()),
                message: Some(format!("entry {}", i)),
                fields: HashMap::new(),
                stream: None,
            });
        }

        assert_eq!(state.scroll, ScrollState::Tail);

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

        state.scroll = scroll_to_bottom(&state.scroll, &state.log_lines);
        assert_eq!(state.scroll, ScrollState::Tail);
    }
}
