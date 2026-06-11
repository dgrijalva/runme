use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        // Down: move sidebar selection down, re-deriving the focus filter.
        KeyCode::Down => {
            state.sidebar.move_down(state.sidebar_entries.len());
            state.refresh_focus_filter();
        }

        // Up: move sidebar selection up, re-deriving the focus filter.
        KeyCode::Up => {
            state.sidebar.move_up();
            state.refresh_focus_filter();
        }

        // ]: jump to the next section-level entry (section header or
        // top-level task). Skips sub-tasks and processes. Stays put when
        // already at the last section-level row.
        KeyCode::Char(']') => {
            if let Some(idx) =
                sidebar::next_section_level(&state.sidebar_entries, state.sidebar.selection)
            {
                state.sidebar.selection = idx;
                state.refresh_focus_filter();
            }
        }

        // [: jump to the previous section-level entry. Symmetric with `]`.
        KeyCode::Char('[') => {
            if let Some(idx) =
                sidebar::prev_section_level(&state.sidebar_entries, state.sidebar.selection)
            {
                state.sidebar.selection = idx;
                state.refresh_focus_filter();
            }
        }

        // Enter: open process detail (for process entries) or toggle source visibility (for task entry).
        // Section headers no-op — they're filter-only rows.
        KeyCode::Enter => {
            if let Some(entry) = state.sidebar_entries.get(state.sidebar.selection) {
                if entry.is_section_header() {
                    // Section headers are no-op for activation keys.
                } else if entry.is_task() {
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

        // Space: toggle source visibility. Section headers no-op.
        KeyCode::Char(' ') => {
            if let Some(entry) = state.sidebar_entries.get(state.sidebar.selection) {
                if entry.is_section_header() {
                    // No-op.
                } else {
                    let source = entry.source;
                    state.toggle_source_visibility(source);
                }
            }
        }

        // s: stop selected process (SIGTERM). Section headers no-op
        // (handled inside `send_signal_to_selected`).
        KeyCode::Char('s') => {
            send_signal_to_selected(state, nix::sys::signal::Signal::SIGTERM);
        }

        // S: send SIGHUP to selected process.
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
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_log_viewer_key(
    key: KeyEvent,
    state: &mut AppState,
    filtered_entries: &[LogEntry],
    viewport_height: u16,
    viewport_width: u16,
    source_labels: &HashMap<TaskId, String>,
) {
    match key.code {
        // Down: move cursor to next entry
        KeyCode::Down => {
            state.scroll = scroll_down(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
                source_labels,
            );
        }

        // Up: move cursor to previous entry
        KeyCode::Up => {
            state.scroll = scroll_up(
                &state.scroll,
                filtered_entries,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
                source_labels,
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
                source_labels,
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
                source_labels,
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
                source_labels,
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
                source_labels,
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

        // /: enter search input mode. Pre-populates with the active pattern.
        KeyCode::Char('/') => {
            state.search.save_current();
            state.mode = AppMode::SearchInput;
        }

        // Esc: clear active filter + search (back to default view).
        // Source hides and focus filter are untouched (out of scope).
        KeyCode::Esc => {
            state.filter_input.clear();
            state.search.clear_active();
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

/// Handle keys when the picker overlay is open.
///
/// Picker is an overlay (decision 1 + 8): Esc closes the overlay
/// without quitting; Ctrl-C still quits the entire TUI.
/// Sets `state.pending_task` when the user picks a task; the event
/// loop spawns it (the new task appears in the sidebar) and the
/// overlay closes.
pub(super) fn handle_picker_key(key: KeyEvent, state: &mut AppState) {
    // Common: Esc closes, Ctrl-C quits, Tab/Shift-Tab toggles focus.
    match key.code {
        KeyCode::Esc => {
            // Persist current input to memory before closing.
            save_picker_input(state);
            // Fresh session with nothing to look at: quit instead of leaving
            // the user in an empty TUI shell with no log history.
            if state.current_task_id.is_none() && state.log_lines.is_empty() {
                state.running = false;
            } else {
                state.close_picker();
            }
            return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.running = false;
            return;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if let Some(ref mut picker) = state.picker {
                picker.focus = match picker.focus {
                    super::picker::PickerFocus::TaskList => super::picker::PickerFocus::ArgsInput,
                    super::picker::PickerFocus::ArgsInput => super::picker::PickerFocus::TaskList,
                };
            }
            return;
        }
        _ => {}
    }

    let focus = match state.picker.as_ref() {
        Some(p) => p.focus,
        None => return,
    };
    match focus {
        super::picker::PickerFocus::TaskList => handle_picker_task_list_key(key, state),
        super::picker::PickerFocus::ArgsInput => handle_picker_args_input_key(key, state),
    }
}

/// Save the picker's current args input to per-task memory.
fn save_picker_input(state: &mut AppState) {
    let Some(ref picker) = state.picker else {
        return;
    };
    let Some(name) = picker.selected_qualified_name() else {
        return;
    };
    state.task_args.insert(name, picker.args_input.clone());
}

/// Refresh picker derived state (cached help, validation, args input) for
/// the currently-selected task. Call after any selection change.
fn refresh_picker_selection(state: &mut AppState) {
    let memory = state.task_args.clone();
    if let Some(ref mut picker) = state.picker {
        picker.refresh_for_selection(&memory);
    }
}

fn handle_picker_task_list_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // Enter: launch selected task with the args currently typed in
        // the right panel (which may have been pre-filled from memory).
        KeyCode::Enter => {
            save_picker_input(state);
            if let Some(ref picker) = state.picker
                && let Some(task) = picker.selected_task()
            {
                let argv = picker.parsed_argv();
                state.pending_task = Some((task, argv));
            }
        }

        KeyCode::Down => {
            if let Some(ref mut picker) = state.picker {
                picker.move_down();
            }
            refresh_picker_selection(state);
        }
        KeyCode::Up => {
            if let Some(ref mut picker) = state.picker {
                picker.move_up();
            }
            refresh_picker_selection(state);
        }

        KeyCode::Backspace => {
            if let Some(ref mut picker) = state.picker {
                picker.delete_char();
            }
            refresh_picker_selection(state);
        }

        // Ctrl-u: clear filter input
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(ref mut picker) = state.picker {
                picker.input.clear();
                picker.cursor = 0;
                picker.selection = 0;
                picker.scroll_offset = 0;
            }
            refresh_picker_selection(state);
        }

        KeyCode::Char(ch) => {
            if let Some(ref mut picker) = state.picker {
                picker.insert_char(ch);
            }
            refresh_picker_selection(state);
        }

        _ => {}
    }
}

fn handle_picker_args_input_key(key: KeyEvent, state: &mut AppState) {
    let Some(ref mut picker) = state.picker else {
        return;
    };
    match key.code {
        // Ctrl-]: scroll help down. Ctrl-[ collides with Esc on most
        // terminals (Esc is the C0 escape), so we also accept PageUp/
        // PageDown as portable fallbacks.
        KeyCode::Char(']') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.scroll_help_down(5);
            return;
        }
        KeyCode::Char('[') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.scroll_help_up(5);
            return;
        }
        KeyCode::PageDown => {
            picker.scroll_help_down(5);
            return;
        }
        KeyCode::PageUp => {
            picker.scroll_help_up(5);
            return;
        }

        KeyCode::Enter => {
            // Save and launch.
            let qual = picker.selected_qualified_name();
            let argv = picker.parsed_argv();
            let task = picker.selected_task();
            if let (Some(name), Some(task)) = (qual, task) {
                state
                    .task_args
                    .insert(name, state.picker.as_ref().unwrap().args_input.clone());
                state.pending_task = Some((task, argv));
            }
            return;
        }

        KeyCode::Backspace => {
            picker.delete_arg_char();
        }
        KeyCode::Left => {
            picker.arg_cursor_left();
        }
        KeyCode::Right => {
            picker.arg_cursor_right();
        }
        KeyCode::Home => {
            picker.arg_cursor_home();
        }
        KeyCode::End => {
            picker.arg_cursor_end();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.args_input.clear();
            picker.args_cursor = 0;
        }
        KeyCode::Char(ch) => {
            picker.insert_arg_char(ch);
        }
        _ => {
            return;
        }
    }

    // After mutating args input, re-validate.
    refresh_picker_selection(state);
}

/// Handle keys when in filter input mode.
pub(super) fn handle_filter_input_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // Enter: commit current text to history (MRU dedup) and close panel.
        KeyCode::Enter => {
            state.filter_input.commit();
            state.mode = AppMode::Normal;
        }

        // Esc: revert to snapshot and close panel.
        KeyCode::Esc => {
            state.filter_input.revert();
            state.mode = AppMode::Normal;
        }

        // Up / Down: navigate the [saved..., virtual, blank] sequence.
        KeyCode::Up => {
            state.filter_input.history_up();
        }
        KeyCode::Down => {
            state.filter_input.history_down();
        }

        // Ctrl-u: clear the input
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.filter_input.clear();
        }

        // Ctrl-c: cancel (revert) and return to Normal mode
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.filter_input.revert();
            state.mode = AppMode::Normal;
        }

        KeyCode::Left => {
            state.filter_input.move_left();
        }
        KeyCode::Right => {
            state.filter_input.move_right();
        }
        KeyCode::Backspace => {
            state.filter_input.delete_char_before();
        }
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
        // Enter: commit current text as the search pattern, scan, jump.
        KeyCode::Enter => {
            state.search.commit();
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
                state
                    .search
                    .scan_matches(texts.iter().map(|(i, t)| (*i, t.as_str())));

                let cursor_pos = state.scroll.cursor_index(filtered_entries).unwrap_or(0);
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

        // Esc: revert to snapshot (active pattern unchanged) and close.
        KeyCode::Esc => {
            state.search.revert();
            state.mode = AppMode::Normal;
        }

        // Up / Down: navigate history with virtual slot.
        KeyCode::Up => {
            state.search.history_up();
        }
        KeyCode::Down => {
            state.search.history_down();
        }

        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.search.clear_input();
        }

        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.search.revert();
            state.mode = AppMode::Normal;
        }

        KeyCode::Left => {
            state.search.move_left();
        }
        KeyCode::Right => {
            state.search.move_right();
        }
        KeyCode::Backspace => {
            state.search.delete_char_before();
        }
        KeyCode::Char(ch) => {
            state.search.insert_char(ch);
        }

        _ => {}
    }
}

/// Handle keys when in entry detail mode.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_detail_key(
    key: KeyEvent,
    state: &mut AppState,
    filtered_entries: &[LogEntry],
    viewport_height: u16,
    viewport_width: u16,
    source_labels: &HashMap<TaskId, String>,
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
                    source_labels,
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
                    source_labels,
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
    // Tasks and section headers can't be signaled — only process entries.
    if entry.is_task() || entry.is_section_header() {
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
    if visible_indices.is_empty() {
        return;
    }
    let visible_entries: Vec<LogEntry> = visible_indices
        .iter()
        .filter_map(|&i| state.log_lines.get(i).cloned())
        .collect();
    let cursor_visible_idx = state
        .scroll
        .cursor_index(&visible_entries)
        .unwrap_or(visible_indices.len() - 1);
    let cursor_idx = visible_indices[cursor_visible_idx];

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
    let visible_entries: Vec<LogEntry> =
        state.visible_log_lines().into_iter().cloned().collect();
    let total = visible_entries.len();
    if total == 0 {
        return;
    }
    let target = target.min(total.saturating_sub(1));

    // If target is the last entry, go to Tail
    if target >= total.saturating_sub(1) {
        state.scroll = viewport::ScrollState::Tail;
    } else {
        // Place the target entry near the middle of the viewport.
        let half = (viewport_height / 2) as usize;
        let new_top = target.saturating_sub(half);
        state.scroll = viewport::ScrollState::pinned(&visible_entries, target, new_top);
    }
}

/// Handle keys in the quit-confirmation modal.
///
/// Design decision 7: prompted only when tasks are still running.
/// Enter confirms (running -> false; the outer driver calls `engine.quit()`
/// when the event loop exits). Esc / anything else dismisses.
pub(super) fn handle_quit_confirm_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        KeyCode::Enter => {
            state.quit_confirm = false;
            state.running = false;
        }
        // Esc / anything else dismisses.
        _ => {
            state.quit_confirm = false;
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

/// Handle keys in the kill menu overlay (design decision 4).
///
/// `k` SIGTERMs the focused task ("kk" = "kill this"); `9` SIGKILLs it;
/// `a` SIGTERMs all direct children of root (`KillAll`). Any other key
/// dismisses, mirroring the copy menu.
///
/// Async engine calls are fire-and-forget via `tokio::spawn` — the handler
/// itself stays sync so it can be invoked from the event-loop key dispatch.
pub(super) fn handle_kill_menu_key(key: KeyEvent, state: &mut AppState) {
    use crate::execution::KillSignal;

    match key.code {
        // `kk` — SIGTERM the focused task.
        KeyCode::Char('k') => {
            kill_focused(state, KillSignal::Term);
            state.mode = AppMode::Normal;
        }
        // `k9` — SIGKILL the focused task.
        KeyCode::Char('9') => {
            kill_focused(state, KillSignal::Kill);
            state.mode = AppMode::Normal;
        }
        // `ka` — KillAll: cancel each direct child of root, root stays.
        KeyCode::Char('a') => {
            if let Some(handle) = state.engine.clone() {
                tokio::spawn(async move {
                    let _ = handle.kill_all().await;
                });
                state
                    .notifications
                    .push(("Killed all tasks".to_string(), std::time::Instant::now()));
            }
            state.mode = AppMode::Normal;
        }
        // Any other key dismisses the menu (mirror copy menu).
        _ => {
            state.mode = AppMode::Normal;
        }
    }
}

/// What the kill menu's `kk`/`k9` should target. Process entries get
/// killed individually (just that process); task entries kill the whole
/// task subtree via `engine.kill_task`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KillTarget {
    Task(crate::execution::TaskId),
    Process(crate::execution::TaskId),
}

/// Resolve the kill target from the current sidebar selection.
///
/// - `None` if a section header ("All tasks", "Running tasks", "Completed
///   tasks") is selected, or selection is empty. Caller should treat as
///   no-op + status hint.
/// - For task entries, returns `Task(entry.source)`.
/// - For process entries, returns `Process(entry.source)` — the engine
///   walks its table to find the owning task and signals just that
///   process group.
pub(super) fn resolve_kill_target(state: &AppState) -> Option<KillTarget> {
    let entry = state.sidebar_entries.get(state.sidebar.selection)?;

    // Section headers (including "All tasks") are never valid kill targets.
    if entry.is_section_header() {
        return None;
    }

    // Defensive: a non-header entry pointing at root would also be invalid.
    if let Some(handle) = state.engine.as_ref()
        && entry.source == handle.root
    {
        return None;
    }

    if entry.is_task() {
        Some(KillTarget::Task(entry.source))
    } else {
        Some(KillTarget::Process(entry.source))
    }
}

/// Fire-and-forget engine kill for whatever's focused in the sidebar.
///
/// Task entries dispatch to `engine.kill_task` (cancels the task subtree);
/// process entries dispatch to `engine.kill_process` (signals just the
/// individual process group, leaving sibling processes and the parent
/// task running). Posts a notification reporting the action.
///
/// No-op (with a status hint) when "All tasks" is selected or when
/// nothing resolves. Mirrors the kill semantics in design decision 4.
fn kill_focused(state: &mut AppState, signal: crate::execution::KillSignal) {
    use crate::execution::KillSignal;

    let Some(target) = resolve_kill_target(state) else {
        state.notifications.push((
            "Select a task to kill, or use `ka` for all".to_string(),
            std::time::Instant::now(),
        ));
        return;
    };
    let Some(handle) = state.engine.clone() else {
        return;
    };

    // Stash a display name for the notification before spawning. For
    // task targets we look up the matching task entry; for process
    // targets we find the process entry by source.
    let entry = state
        .sidebar_entries
        .iter()
        .find(|e| {
            e.source
                == match target {
                    KillTarget::Task(id) | KillTarget::Process(id) => id,
                }
        })
        .cloned();
    let name = entry
        .as_ref()
        .map(|e| e.name.clone())
        .unwrap_or_else(|| match target {
            KillTarget::Task(id) => format!("task {}", id.0),
            KillTarget::Process(id) => format!("process {}", id.0),
        });

    let signal_label = match signal {
        KillSignal::Term => "SIGTERM",
        KillSignal::Kill => "SIGKILL",
    };

    match target {
        KillTarget::Task(id) => {
            tokio::spawn(async move {
                let _ = handle.kill_task(id, signal).await;
            });
        }
        KillTarget::Process(id) => {
            tokio::spawn(async move {
                let _ = handle.kill_process(id, signal).await;
            });
        }
    }

    state.notifications.push((
        format!("Sent {} to {}", signal_label, name),
        std::time::Instant::now(),
    ));
}

/// Copy all entries currently visible on screen (viewport) to clipboard via OSC 52.
fn copy_viewport_to_clipboard(state: &mut AppState) {
    let visible_indices = state.visible_line_indices();
    if visible_indices.is_empty() {
        return;
    }
    let visible_entries: Vec<LogEntry> = visible_indices
        .iter()
        .filter_map(|&i| state.log_lines.get(i).cloned())
        .collect();

    // Determine the viewport range from scroll state
    let height = state.last_viewport_height.unwrap_or(40) as usize;
    let (start, end) = match state.scroll {
        viewport::ScrollState::Tail => {
            let end = visible_indices.len();
            let start = end.saturating_sub(height);
            (start, end)
        }
        viewport::ScrollState::Pinned { .. } => {
            let top = state
                .scroll
                .resolve(&visible_entries)
                .map(|r| r.top)
                .unwrap_or(0);
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
    let visible_entries: Vec<LogEntry> = visible_indices
        .iter()
        .filter_map(|&i| state.log_lines.get(i).cloned())
        .collect();
    let cursor_visible_idx = state
        .scroll
        .cursor_index(&visible_entries)
        .unwrap_or(visible_indices.len() - 1);
    let cursor_idx = visible_indices[cursor_visible_idx];

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

    use super::super::sidebar::{SidebarEntry, SidebarEntryKind};
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
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source,
            seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(raw.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    #[test]
    fn sidebar_arrows_move_selection() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar_entries = vec![
            SidebarEntry {
                name: "task".to_string(),
                source: TaskId(1),
                status_tag: "SETUP".to_string(),
                status_color: Color::Yellow,
                visible: true,
                kind: SidebarEntryKind::Task,
                depth: 0,
            },
            SidebarEntry {
                name: "api".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                kind: SidebarEntryKind::Process,
                depth: 1,
            },
            SidebarEntry {
                name: "worker".to_string(),
                source: TaskId(3),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                kind: SidebarEntryKind::Process,
                depth: 1,
            },
        ];

        assert_eq!(state.sidebar.selection, 0);
        handle_sidebar_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 1);
        handle_sidebar_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 2);
        // Clamp at max
        handle_sidebar_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 2);
        // Move back up
        handle_sidebar_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
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
                kind: SidebarEntryKind::Task,
                depth: 0,
            },
            SidebarEntry {
                name: "api".to_string(),
                source: api_id,
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                kind: SidebarEntryKind::Process,
                depth: 1,
            },
        ];

        let labels = HashMap::new();
        // Press '2' — hides sidebar_entries[1] (api).
        handle_log_viewer_key(
            make_key_event(KeyCode::Char('2'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert!(state.hidden_sources.contains(&api_id));
        assert!(!state.hidden_sources.contains(&task_id));
    }

    #[test]
    fn a_key_shows_all() {
        let mut state = AppState::new();
        state.hidden_sources.insert(TaskId(2));
        assert!(!state.hidden_sources.is_empty());

        let labels = HashMap::new();
        handle_log_viewer_key(
            make_key_event(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert!(state.hidden_sources.is_empty());
    }

    // -- Entry detail view tests --

    #[test]
    fn enter_opens_detail_view() {
        let mut state = AppState::new();
        state.log_lines.push(make_log_entry("hello", TaskId(1)));
        // Pin to entry 0 so there's a cursor
        state.scroll = ScrollState::pinned(&state.log_lines, 0, 0);

        let entries = state.log_lines.clone();
        assert_eq!(state.mode, AppMode::Normal);
        let labels = HashMap::new();
        handle_log_viewer_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &entries,
            24,
            80,
            &labels,
        );
        assert_eq!(state.mode, AppMode::EntryDetail);
        assert_eq!(state.detail_scroll, 0);
    }

    #[test]
    fn enter_does_nothing_with_no_entries() {
        let mut state = AppState::new();
        assert_eq!(state.mode, AppMode::Normal);
        let labels = HashMap::new();
        handle_log_viewer_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        // Should stay in Normal mode since there are no visible entries
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn detail_esc_closes() {
        let mut state = AppState::new();
        state.log_lines.push(make_log_entry("hello", TaskId(1)));
        state.mode = AppMode::EntryDetail;

        let labels = HashMap::new();
        handle_detail_key(
            make_key_event(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn detail_q_closes() {
        let mut state = AppState::new();
        state.mode = AppMode::EntryDetail;

        let labels = HashMap::new();
        handle_detail_key(
            make_key_event(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn detail_jk_scrolls() {
        let mut state = AppState::new();
        state.mode = AppMode::EntryDetail;
        state.detail_scroll = 0;
        let labels = HashMap::new();

        // j scrolls down
        handle_detail_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert_eq!(state.detail_scroll, 1);

        handle_detail_key(
            make_key_event(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert_eq!(state.detail_scroll, 2);

        // k scrolls up
        handle_detail_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
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
            &labels,
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
        state.scroll = ScrollState::pinned(&state.log_lines, 2, 0);
        state.mode = AppMode::EntryDetail;

        let entries = state.log_lines.clone();
        let labels = HashMap::new();
        handle_detail_key(
            make_key_event(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut state,
            &entries,
            24,
            80,
            &labels,
        );
        assert_eq!(state.mode, AppMode::Normal);
        // Should have moved cursor down (from 2 to 3); Tail (None) is also OK at end of list
        if let Some(idx) = state.scroll.cursor_index(&state.log_lines) {
            assert_eq!(idx, 3);
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
        state.scroll = ScrollState::pinned(&state.log_lines, 2, 0);
        state.mode = AppMode::EntryDetail;

        let entries = state.log_lines.clone();
        let labels = HashMap::new();
        handle_detail_key(
            make_key_event(KeyCode::Char('N'), KeyModifiers::NONE),
            &mut state,
            &entries,
            24,
            80,
            &labels,
        );
        assert_eq!(state.mode, AppMode::Normal);
        // Should have moved cursor up (from 2 to 1)
        let idx = state
            .scroll
            .cursor_index(&state.log_lines)
            .expect("expected Pinned");
        assert_eq!(idx, 1);
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
            kind: SidebarEntryKind::Task,
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
                kind: SidebarEntryKind::Task,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                kind: SidebarEntryKind::Process,
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
                kind: SidebarEntryKind::Task,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                kind: SidebarEntryKind::Process,
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
                kind: SidebarEntryKind::Task,
                depth: 0,
            },
            SidebarEntry {
                name: "echo hello".to_string(),
                source: TaskId(2),
                status_tag: "RUN".to_string(),
                status_color: Color::Green,
                visible: true,
                kind: SidebarEntryKind::Process,
                depth: 1,
            },
        ];

        handle_sidebar_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    // -- Section header guard tests --
    //
    // Section headers ("All tasks" / "Running tasks" / "Completed tasks")
    // are filter-driving rows: navigation works, but Enter/Space/s/S no-op.

    fn make_header_entry(name: &str, kind: SidebarEntryKind) -> SidebarEntry {
        SidebarEntry {
            name: name.to_string(),
            source: TaskId::ROOT,
            status_tag: String::new(),
            status_color: Color::Gray,
            visible: true,
            kind,
            depth: 0,
        }
    }

    #[test]
    fn sidebar_enter_on_section_header_is_noop() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar_entries = vec![
            make_header_entry("All tasks", SidebarEntryKind::AllTasks),
            make_header_entry("Running tasks", SidebarEntryKind::RunningHeader),
            make_header_entry("Completed tasks", SidebarEntryKind::CompletedHeader),
        ];
        for sel in 0..3 {
            state.sidebar.selection = sel;
            state.mode = AppMode::Normal;
            handle_sidebar_key(
                make_key_event(KeyCode::Enter, KeyModifiers::NONE),
                &mut state,
            );
            // Mode should not have flipped to ProcessDetail; visibility
            // should not have toggled either.
            assert_eq!(state.mode, AppMode::Normal);
            assert!(state.hidden_sources.is_empty());
        }
    }

    #[test]
    fn sidebar_space_on_section_header_is_noop() {
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar_entries =
            vec![make_header_entry("Running tasks", SidebarEntryKind::RunningHeader)];
        state.sidebar.selection = 0;
        handle_sidebar_key(
            make_key_event(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.hidden_sources.is_empty());
    }

    #[test]
    fn sidebar_s_on_section_header_is_noop() {
        // No engine + section header selected: should not panic, should
        // not attempt any signal dispatch.
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar_entries = vec![make_header_entry(
            "Completed tasks",
            SidebarEntryKind::CompletedHeader,
        )];
        state.sidebar.selection = 0;
        handle_sidebar_key(
            make_key_event(KeyCode::Char('s'), KeyModifiers::NONE),
            &mut state,
        );
        handle_sidebar_key(
            make_key_event(KeyCode::Char('S'), KeyModifiers::NONE),
            &mut state,
        );
        // No crash + no state change is the test here.
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn sidebar_arrows_navigate_through_section_headers() {
        // Down should let selection advance through section header rows
        // even though they're not toggleable.
        let mut state = AppState::new();
        state.sidebar.focused = true;
        state.sidebar_entries = vec![
            make_header_entry("All tasks", SidebarEntryKind::AllTasks),
            make_header_entry("Running tasks", SidebarEntryKind::RunningHeader),
            make_header_entry("Completed tasks", SidebarEntryKind::CompletedHeader),
        ];
        assert_eq!(state.sidebar.selection, 0);
        handle_sidebar_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 1);
        handle_sidebar_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.sidebar.selection, 2);
    }

    #[test]
    fn resolve_kill_target_section_header_is_none() {
        let mut state = AppState::new();
        state.sidebar_entries = vec![
            make_header_entry("All tasks", SidebarEntryKind::AllTasks),
            make_header_entry("Running tasks", SidebarEntryKind::RunningHeader),
            make_header_entry("Completed tasks", SidebarEntryKind::CompletedHeader),
        ];
        for sel in 0..3 {
            state.sidebar.selection = sel;
            assert_eq!(resolve_kill_target(&state), None);
        }
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
        let labels = HashMap::new();

        handle_log_viewer_key(
            make_key_event(KeyCode::Char('\\'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert!(!state.sidebar_visible);

        handle_log_viewer_key(
            make_key_event(KeyCode::Char('\\'), KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &labels,
        );
        assert!(state.sidebar_visible);
    }

    // -- Filter history tests --

    #[test]
    fn filter_history_saved_on_confirm() {
        let mut state = AppState::new();
        state.mode = AppMode::FilterInput;

        for ch in "error".chars() {
            state.filter_input.insert_char(ch);
        }

        handle_filter_input_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );

        assert_eq!(state.filter_input.input.history, vec!["error".to_string()]);
    }

    #[test]
    fn filter_history_up_down_cycles() {
        let mut state = AppState::new();
        state.filter_input.input.history = vec!["error".to_string(), "level:warn".to_string()];
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();

        // Up should go to the newest saved entry
        handle_filter_input_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
        assert_eq!(state.filter_input.input.text, "level:warn");

        // Up again should go to the older saved entry
        handle_filter_input_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
        assert_eq!(state.filter_input.input.text, "error");

        // Down should go back forward through history
        handle_filter_input_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.filter_input.input.text, "level:warn");

        // Down again drops back to the virtual slot (empty in this session)
        handle_filter_input_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.filter_input.input.text.is_empty());
    }

    #[test]
    fn filter_history_nav_updates_active_filter() {
        // Regression: arrow navigation through history must update
        // `last_valid_expr` so that pressing Enter applies the selected filter.
        let mut state = AppState::new();
        state.filter_input.input.history = vec!["level:warn".to_string()];
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();
        assert!(state.filter_input.active_expr().is_none());

        handle_filter_input_key(make_key_event(KeyCode::Up, KeyModifiers::NONE), &mut state);
        assert_eq!(state.filter_input.input.text, "level:warn");
        assert!(
            state.filter_input.active_expr().is_some(),
            "history nav must reparse so the active filter matches the displayed text"
        );

        // Down drops to virtual (empty); active filter clears.
        handle_filter_input_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.filter_input.input.text.is_empty());
        assert!(state.filter_input.active_expr().is_none());
    }

    #[test]
    fn filter_history_down_from_active_clears_immediately() {
        // Bug fix: f → Down should clear an active filter in one keystroke
        // (no need for f → Up → Down → Down detour). With the virtual-slot
        // model, save_current seeds position=Virtual and Down → Blank.
        let mut state = AppState::new();
        state.filter_input.input.history = vec!["error".to_string()];
        // Pretend a filter is already active.
        for ch in "error".chars() {
            state.filter_input.insert_char(ch);
        }
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();
        assert_eq!(state.filter_input.input.text, "error");

        handle_filter_input_key(
            make_key_event(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.filter_input.input.text.is_empty());
        assert!(state.filter_input.active_expr().is_none());
    }

    #[test]
    fn filter_esc_reverts_to_snapshot() {
        let mut state = AppState::new();
        for ch in "error".chars() {
            state.filter_input.insert_char(ch);
        }
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();

        // Mutate the input
        state.filter_input.clear();
        for ch in "warn".chars() {
            state.filter_input.insert_char(ch);
        }
        assert_eq!(state.filter_input.input.text, "warn");

        // Esc reverts to snapshot
        handle_filter_input_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert_eq!(state.filter_input.input.text, "error");
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn filter_history_mru_dedup_on_recommit() {
        // Re-applying a saved filter should move it to newest, not duplicate.
        let mut state = AppState::new();
        state.mode = AppMode::FilterInput;

        // Commit "foo"
        for ch in "foo".chars() {
            state.filter_input.insert_char(ch);
        }
        handle_filter_input_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );

        // Open again, commit "bar"
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();
        state.filter_input.clear();
        for ch in "bar".chars() {
            state.filter_input.insert_char(ch);
        }
        handle_filter_input_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );

        // Open again, re-commit "foo"
        state.mode = AppMode::FilterInput;
        state.filter_input.save_current();
        state.filter_input.clear();
        for ch in "foo".chars() {
            state.filter_input.insert_char(ch);
        }
        handle_filter_input_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );

        assert_eq!(
            state.filter_input.input.history,
            vec!["bar".to_string(), "foo".to_string()]
        );
    }

    #[test]
    fn esc_from_normal_clears_filter_and_search() {
        let mut state = AppState::new();
        // Active filter
        for ch in "error".chars() {
            state.filter_input.insert_char(ch);
        }
        assert!(state.filter_input.has_active_filter());
        // Active search
        state.search.pattern = "needle".to_string();
        state.search.active = true;

        handle_log_viewer_key(
            make_key_event(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &[],
            24,
            80,
            &HashMap::new(),
        );

        assert!(!state.filter_input.has_active_filter());
        assert!(state.filter_input.input.text.is_empty());
        assert!(!state.search.active);
        assert!(state.search.pattern.is_empty());
    }

    // -- Picker overlay tests --

    #[test]
    fn picker_esc_closes_overlay_keeps_running() {
        let mut state = AppState::new();
        state.picker_open = true;
        state.picker = Some(super::super::picker::PickerState::new(&[], &HashMap::new()));
        // Simulate "user opened picker via `t` after running something" — a task
        // has been launched in this session, so Esc should just close the overlay.
        state.current_task_id = Some(TaskId(1));
        assert!(state.running);

        handle_picker_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(!state.picker_open);
        assert!(state.picker.is_none());
        assert!(state.running, "Esc on picker must not quit the TUI");
    }

    #[test]
    fn picker_esc_on_empty_session_quits() {
        // Fresh launch with no args lands directly in the picker over an empty
        // TUI. Esc here has no useful destination — it should quit.
        let mut state = AppState::new();
        state.picker_open = true;
        state.picker = Some(super::super::picker::PickerState::new(&[], &HashMap::new()));
        assert!(state.current_task_id.is_none());
        assert!(state.log_lines.is_empty());

        handle_picker_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(!state.running, "Esc on empty initial picker must quit");
    }

    #[test]
    fn picker_q_does_not_quit() {
        let mut state = AppState::new();
        state.picker_open = true;
        state.picker = Some(super::super::picker::PickerState::new(&[], &HashMap::new()));

        // 'q' in the picker is just an input character, not a quit.
        handle_picker_key(
            make_key_event(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(state.running, "q in picker must not quit");
        assert!(state.picker_open, "q in picker must not close overlay");
    }

    #[test]
    fn picker_ctrl_c_quits() {
        let mut state = AppState::new();
        state.picker_open = true;
        state.picker = Some(super::super::picker::PickerState::new(&[], &HashMap::new()));

        handle_picker_key(
            make_key_event(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert!(!state.running);
    }

    // -- Quit-confirm modal tests --

    #[test]
    fn quit_confirm_enter_quits() {
        let mut state = AppState::new();
        state.quit_confirm = true;
        handle_quit_confirm_key(
            make_key_event(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert!(!state.quit_confirm);
        assert!(!state.running);
    }

    #[test]
    fn quit_confirm_other_key_dismisses() {
        let mut state = AppState::new();
        state.quit_confirm = true;
        handle_quit_confirm_key(
            make_key_event(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut state,
        );
        assert!(!state.quit_confirm);
        assert!(state.running);
    }

    #[test]
    fn quit_confirm_esc_dismisses() {
        let mut state = AppState::new();
        state.quit_confirm = true;
        handle_quit_confirm_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(!state.quit_confirm);
        assert!(state.running);
    }

    // -- Kill menu tests --

    fn make_task_entry(name: &str, id: TaskId) -> SidebarEntry {
        SidebarEntry {
            name: name.to_string(),
            source: id,
            status_tag: "RUN".to_string(),
            status_color: Color::Green,
            visible: true,
            kind: SidebarEntryKind::Task,
            depth: 0,
        }
    }

    fn make_proc_entry(name: &str, id: TaskId) -> SidebarEntry {
        SidebarEntry {
            name: name.to_string(),
            source: id,
            status_tag: "RUN".to_string(),
            status_color: Color::Green,
            visible: true,
            kind: SidebarEntryKind::Process,
            depth: 1,
        }
    }

    #[test]
    fn kill_menu_esc_dismisses() {
        let mut state = AppState::new();
        state.mode = AppMode::KillMenu;
        handle_kill_menu_key(make_key_event(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn kill_menu_unknown_key_dismisses() {
        // Mirror copy menu: any unrecognized key closes the menu.
        let mut state = AppState::new();
        state.mode = AppMode::KillMenu;
        handle_kill_menu_key(
            make_key_event(KeyCode::Char('z'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn kill_menu_kk_returns_to_normal() {
        // Without an engine, `kk` is still safe — just no async dispatch.
        let mut state = AppState::new();
        state.mode = AppMode::KillMenu;
        state.sidebar_entries = vec![make_task_entry("api", TaskId(2))];
        state.sidebar.selection = 0;
        handle_kill_menu_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn kill_menu_k9_returns_to_normal() {
        let mut state = AppState::new();
        state.mode = AppMode::KillMenu;
        state.sidebar_entries = vec![make_task_entry("api", TaskId(2))];
        state.sidebar.selection = 0;
        handle_kill_menu_key(
            make_key_event(KeyCode::Char('9'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn kill_menu_ka_returns_to_normal() {
        let mut state = AppState::new();
        state.mode = AppMode::KillMenu;
        handle_kill_menu_key(
            make_key_event(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
    }

    #[test]
    fn kill_menu_kk_with_no_selection_posts_hint() {
        // No engine + no entries: resolve_kill_target returns None,
        // kill_focused posts a hint notification rather than panicking.
        let mut state = AppState::new();
        state.mode = AppMode::KillMenu;
        let initial_notifs = state.notifications.len();
        handle_kill_menu_key(
            make_key_event(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.mode, AppMode::Normal);
        assert!(state.notifications.len() > initial_notifs);
    }

    #[test]
    fn resolve_kill_target_task_entry() {
        // Selecting a task entry should resolve to KillTarget::Task with
        // the entry's id (engine-less: the root check short-circuits via
        // handle absence).
        let mut state = AppState::new();
        state.sidebar_entries = vec![make_task_entry("api", TaskId(7))];
        state.sidebar.selection = 0;
        assert_eq!(
            resolve_kill_target(&state),
            Some(KillTarget::Task(TaskId(7)))
        );
    }

    #[test]
    fn resolve_kill_target_process_entry() {
        // Process entries now resolve to KillTarget::Process — the
        // engine signals just that process group, not its parent task.
        let mut state = AppState::new();
        state.sidebar_entries = vec![make_proc_entry("echo hello", TaskId(9))];
        state.sidebar.selection = 0;
        assert_eq!(
            resolve_kill_target(&state),
            Some(KillTarget::Process(TaskId(9)))
        );
    }

    #[test]
    fn resolve_kill_target_empty_selection_is_none() {
        let state = AppState::new();
        // No sidebar_entries — selection out of bounds.
        assert_eq!(resolve_kill_target(&state), None);
    }
}
