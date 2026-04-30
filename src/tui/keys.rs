use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[allow(unused_imports)]
use crate::execution::TaskId;
use crate::log::LogEntry;

use super::app::{AppMode, AppState};
use super::render::DisplayMode;
use super::sidebar;
use super::viewport::{
    self, scroll_down, scroll_down_half_page, scroll_to_bottom, scroll_to_top, scroll_up,
    scroll_up_half_page,
};

/// Handle keys when sidebar is focused.
pub(super) fn handle_sidebar_key(key: KeyEvent, state: &mut AppState) {
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
                    let source = entry.source;
                    state.toggle_source_visibility(source);
                } else {
                    state.process_detail_index = Some(state.sidebar.selection);
                    state.process_detail_scroll = 0;
                    state.process_detail_sockets = None;
                    state.mode = AppMode::ProcessDetail;
                }
            }
        }

        // Space: toggle source visibility
        KeyCode::Char(' ') => {
            if let Some(entry) = state.sidebar_entries.get(state.sidebar.selection) {
                let source = entry.source;
                state.toggle_source_visibility(source);
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

        // a: show all sources
        KeyCode::Char('a') => {
            state.show_all_sources();
        }

        // 1-9: toggle source N
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if let Some(source) = sidebar::source_for_index(&state.sidebar_entries, idx) {
                state.toggle_source_visibility(source);
            }
        }

        _ => {}
    }
}

/// Handle keys when log viewer is focused.
pub(super) fn handle_log_viewer_key(
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

        // d: toggle field details
        KeyCode::Char('d') => {
            state.show_fields = !state.show_fields;
        }

        // \: toggle sidebar visibility
        KeyCode::Char('\\') => {
            state.sidebar_visible = !state.sidebar_visible;
        }

        // y: copy raw entry text to clipboard via OSC 52
        KeyCode::Char('y') => {
            copy_entry_to_clipboard(state);
        }

        // c: open copy menu
        KeyCode::Char('c') => {
            state.mode = AppMode::CopyMenu;
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
                navigate_to_entry(
                    state,
                    target,
                    filtered_entries,
                    viewport_height,
                    viewport_width,
                );
            }
        }

        // N: jump to previous search match
        KeyCode::Char('N') => {
            if state.search.active
                && let Some(target) = state.search.prev_match()
            {
                navigate_to_entry(
                    state,
                    target,
                    filtered_entries,
                    viewport_height,
                    viewport_width,
                );
            }
        }

        // 1-9: toggle source N
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            if let Some(source) = sidebar::source_for_index(&state.sidebar_entries, idx) {
                state.toggle_source_visibility(source);
            }
        }

        _ => {}
    }
}

/// Handle keys when in task picker mode.
///
/// Returns `Some(task)` if the user selected a task, `None` otherwise.
/// The caller (event loop) is responsible for launching the task.
pub(super) fn handle_picker_key(key: KeyEvent, state: &mut AppState) {
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
pub(super) fn handle_filter_input_key(key: KeyEvent, state: &mut AppState) {
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
                        if i > 0 {
                            i - 1
                        } else {
                            0
                        }
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
pub(super) fn handle_search_input_key(
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
                state
                    .search
                    .scan_matches(texts.iter().map(|(i, t)| (*i, t.as_str())));

                // Jump to the nearest match from the current cursor position
                let cursor_pos = match state.scroll {
                    viewport::ScrollState::Tail => {
                        let visible_count = state.visible_line_indices().len();
                        if visible_count > 0 {
                            visible_count - 1
                        } else {
                            0
                        }
                    }
                    viewport::ScrollState::Pinned { cursor, .. } => cursor,
                };
                if let Some(target) = state.search.jump_to_nearest(cursor_pos) {
                    navigate_to_entry(
                        state,
                        target,
                        filtered_entries,
                        viewport_height,
                        viewport_width,
                    );
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

/// Handle keys when in entry detail mode.
pub(super) fn handle_detail_key(
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
                    navigate_to_entry(
                        state,
                        target,
                        filtered_entries,
                        viewport_height,
                        viewport_width,
                    );
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
                    navigate_to_entry(
                        state,
                        target,
                        filtered_entries,
                        viewport_height,
                        viewport_width,
                    );
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

/// Handle keys when in process detail mode.
pub(super) fn handle_process_detail_key(key: KeyEvent, state: &mut AppState) {
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

/// Send a signal to the process corresponding to the selected sidebar entry.
///
/// Sends the signal to the process group (pgid) if available, falling back to
/// the individual pid. If SIGTERM is sent, updates the process status to Stopped.
pub(super) fn send_signal_to_selected(state: &mut AppState, sig: nix::sys::signal::Signal) {
    use nix::sys::signal;
    use nix::unistd::Pid;

    let selection = state.sidebar.selection;
    let entry = match state.sidebar_entries.get(selection) {
        Some(e) => e,
        None => return,
    };
    if entry.is_task {
        return;
    }
    // Find the process in the engine's graph snapshot, matched by id.
    let Some(handle) = state.engine.as_ref() else {
        return;
    };
    let snapshot = handle.graph.borrow().clone();
    for node in snapshot.tasks.values() {
        for proc in &node.processes {
            if proc.id == entry.source {
                let target_pid = if let Some(pgid) = proc.pgid {
                    Some(Pid::from_raw(-pgid))
                } else {
                    proc.pid.map(|pid| Pid::from_raw(pid as i32))
                };
                if let Some(pid) = target_pid {
                    let _ = signal::kill(pid, sig);
                }
                return;
            }
        }
    }
}

/// Send a signal to the process currently being viewed in the process detail panel.
pub(super) fn send_signal_to_process_detail(state: &mut AppState, sig: nix::sys::signal::Signal) {
    // Temporarily set the sidebar selection to the process detail index
    // and delegate to the existing send_signal_to_selected function.
    if let Some(idx) = state.process_detail_index {
        let saved_selection = state.sidebar.selection;
        state.sidebar.selection = idx;
        send_signal_to_selected(state, sig);
        state.sidebar.selection = saved_selection;
    }
}

/// Copy the currently focused entry's raw text to the clipboard using OSC 52 escape sequence.
pub(super) fn copy_entry_to_clipboard(state: &AppState) {
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

/// Export visible log entries to a file.
pub(super) fn export_visible_log(state: &mut AppState) {
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
            state
                .notifications
                .push((format!("Export failed: {}", e), std::time::Instant::now()));
        }
    }
    state.dirty = true;
}

/// Navigate to a specific visible entry index, updating the scroll state.
pub(super) fn navigate_to_entry(
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
        let visible_entries: Vec<LogEntry> =
            state.visible_log_lines().into_iter().cloned().collect();
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

/// Handle keys in the copy menu overlay.
pub(super) fn handle_copy_menu_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        KeyCode::Char('v') => {
            copy_viewport_to_clipboard(state);
            state.mode = AppMode::Normal;
        }
        KeyCode::Char('s') => {
            copy_stream_to_clipboard(state);
            state.mode = AppMode::Normal;
        }
        KeyCode::Char('a') => {
            copy_all_to_clipboard(state);
            state.mode = AppMode::Normal;
        }
        // Any other key dismisses the menu
        _ => {
            state.mode = AppMode::Normal;
        }
    }
}

/// Copy all entries currently visible on screen (viewport) to clipboard via OSC 52.
fn copy_viewport_to_clipboard(state: &mut AppState) {
    let visible_indices = state.visible_line_indices();
    if visible_indices.is_empty() {
        return;
    }

    // Determine the viewport range from scroll state
    let (start, end) = match state.scroll {
        viewport::ScrollState::Tail => {
            let end = visible_indices.len();
            let height = state.last_viewport_height.unwrap_or(40) as usize;
            let start = end.saturating_sub(height);
            (start, end)
        }
        viewport::ScrollState::Pinned { top, .. } => {
            let height = state.last_viewport_height.unwrap_or(40) as usize;
            let end = (top + height).min(visible_indices.len());
            (top, end)
        }
    };

    let content: String = visible_indices[start..end]
        .iter()
        .filter_map(|&idx| state.log_lines.get(idx))
        .map(|entry| entry.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let count = end - start;
    osc52_copy(&content);
    state.notifications.push((
        format!("Copied {} viewport entries", count),
        std::time::Instant::now(),
    ));
    state.dirty = true;
}

/// Copy all entries for the selected source to clipboard via OSC 52.
fn copy_stream_to_clipboard(state: &mut AppState) {
    let visible_indices = state.visible_line_indices();
    if visible_indices.is_empty() {
        return;
    }

    // Determine the source of the currently selected entry
    let cursor_idx = match state.scroll {
        viewport::ScrollState::Tail => *visible_indices.last().unwrap(),
        viewport::ScrollState::Pinned { cursor, .. } => {
            if cursor >= visible_indices.len() {
                *visible_indices.last().unwrap()
            } else {
                visible_indices[cursor]
            }
        }
    };

    let source = match state.log_lines.get(cursor_idx) {
        Some(entry) => entry.source,
        None => return,
    };

    let entries: Vec<&str> = visible_indices
        .iter()
        .filter_map(|&idx| state.log_lines.get(idx))
        .filter(|entry| entry.source == source)
        .map(|entry| entry.raw.as_str())
        .collect();

    let count = entries.len();
    let content = entries.join("\n");
    osc52_copy(&content);
    state.notifications.push((
        format!("Copied {} entries from {}", count, source),
        std::time::Instant::now(),
    ));
    state.dirty = true;
}

/// Copy all entries matching the current filter to clipboard via OSC 52.
fn copy_all_to_clipboard(state: &mut AppState) {
    let visible = state.visible_log_lines();
    if visible.is_empty() {
        return;
    }

    let count = visible.len();
    let content: String = visible
        .iter()
        .map(|entry| entry.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    osc52_copy(&content);
    state.notifications.push((
        format!("Copied {} entries", count),
        std::time::Instant::now(),
    ));
    state.dirty = true;
}

/// Write a string to the system clipboard via OSC 52 escape sequence.
fn osc52_copy(content: &str) {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    let osc52 = format!("\x1b]52;c;{}\x07", encoded);
    let _ = std::io::stdout().write_all(osc52.as_bytes());
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::super::sidebar::SidebarEntry;
    use super::super::viewport::ScrollState;
    use super::*;
    use ratatui::style::Color;

    fn make_key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_log_entry(raw: &str, source: TaskId) -> LogEntry {
        use crate::log::ParsedContent;
        use std::collections::HashMap;

        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source,
            seq: 0,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(raw.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    #[test]
    fn sidebar_jk_moves_selection() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: TaskId(1),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "api".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
            },
            SidebarEntry {
                name: "worker".to_string(),
                source: TaskId(3),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
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
        let task_id = TaskId(1);
        let api_id = TaskId(2);
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: task_id,
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "api".to_string(),
                source: api_id,
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
            },
        ];

        handle_log_viewer_key(
            make_key_event(KeyCode::Char('2'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
        );
        assert!(state.source_filter.contains(&task_id));
        assert!(!state.source_filter.contains(&api_id));
    }

    #[test]
    fn a_key_shows_all() {
        let mut state = AppState::new();
        state.source_filter.insert(TaskId(2));
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

    #[test]
    fn enter_opens_detail_view() {
        let mut state = AppState::new();
        state.log_lines.push(make_log_entry("hello", TaskId(1)));
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
        state.log_lines.push(make_log_entry("hello", TaskId(1)));
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
            state
                .log_lines
                .push(make_log_entry(&format!("entry {}", i), TaskId(1)));
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
            state
                .log_lines
                .push(make_log_entry(&format!("entry {}", i), TaskId(1)));
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
            source: TaskId(7),
            status_tag: "SETUP".to_string(),
            status_color: Color::Yellow,
            visible: true,
            is_task: true,
            depth: 0,
        }];

        // This should be a no-op (task entries can't be signaled)
        handle_sidebar_key(
            make_key_event(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut state,
        );
        // No crash is the test here
    }

    #[test]
    fn sidebar_s_without_engine_is_noop() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar.selection = 1;
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "test".to_string(),
                source: TaskId(1),
                status_tag: "READY".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
            },
        ];
        // No engine wired — should be a no-op
        handle_sidebar_key(
            make_key_event(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut state,
        );
    }

    // -- Process detail tests --

    #[test]
    fn sidebar_enter_on_process_opens_detail() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar.selection = 1;
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: TaskId(1),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
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
        state.sidebar.selection = 0;
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: TaskId(1),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                is_task: true,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                is_task: false,
                depth: 1,
            },
        ];

        handle_sidebar_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn process_detail_esc_closes() {
        let mut state = AppState::new();
        state.mode = AppMode::ProcessDetail;
        state.process_detail_index = Some(1);

        handle_process_detail_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
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
        handle_filter_input_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
        assert_eq!(state.filter_input.text, "level:warn");
        assert_eq!(state.filter_history_index, Some(1));

        // Up again should go to first entry
        handle_filter_input_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
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
        handle_filter_input_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
        assert_eq!(state.filter_history_index, Some(0));

        // Esc should reset the history index
        handle_filter_input_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(state.filter_history_index.is_none());
    }
}
