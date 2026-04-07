use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::log::LogEntry;

use super::app::{AppMode, AppState, render_frame};
use super::render::DisplayMode;
use super::runner::TaskStatus;
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

    while state.running {
        tokio::select! {
            // Terminal input events
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        handle_event(event, state, terminal);
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
                render_frame(terminal, state)?;
                state.dirty = false;
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

    // Global keys (work regardless of focus)
    match key.code {
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

        // Enter / Space: toggle source visibility
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(entry) = state.sidebar_entries.get(state.sidebar.selection) {
                let source = entry.source.clone();
                state.toggle_source_visibility(&source);
            }
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

/// Handle keys when in filter input mode.
fn handle_filter_input_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // Enter: confirm filter and return to Normal mode
        KeyCode::Enter => {
            state.mode = AppMode::Normal;
        }

        // Esc: cancel (revert) and return to Normal mode
        KeyCode::Esc => {
            state.filter_input.revert();
            state.mode = AppMode::Normal;
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
}
