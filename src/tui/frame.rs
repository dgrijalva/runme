use std::io;

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

use crate::theme::THEME;

use super::app::{AppMode, AppState};
use super::filter::{filter_status_spans, render_filter_input};
use super::picker;
use super::render::DisplayMode;
use super::search::{render_search_input, search_status_spans};
use super::sidebar::{self, SIDEBAR_WIDTH};
use super::viewport::{self, ScrollState, new_entries_since_pin};
use crate::log::LogEntry;

/// Render a single frame. Draws the sidebar (left), log viewer (right), and
/// status bar (bottom). Picker and quit-confirmation are layered overlays.
pub fn render_frame(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
) -> io::Result<()> {
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

        // Horizontal layout: sidebar (fixed width) + log viewer (fills).
        // We render the sidebar whenever the engine has any non-root task —
        // i.e. whenever there's something to show.
        let has_task = !state.sidebar_entries.is_empty();
        let show_sidebar = has_task && state.sidebar_visible;
        let horiz_chunks = if show_sidebar {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
                .split(content_area)
        } else {
            // No task running or sidebar collapsed — full-width log viewer
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(0), Constraint::Min(0)])
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
        state.last_viewport_height = Some(log_height);

        // Build the filtered log lines for display
        let visible_entries: Vec<&LogEntry> = state.visible_log_lines();

        let lines: Vec<Line> = if visible_entries.is_empty() {
            if has_task {
                if state.log_lines.is_empty() {
                    vec![Line::from(Span::styled(
                        "  Waiting for output...",
                        Style::default().fg(THEME.dim),
                    ))]
                } else if state.filter_input.has_active_filter() {
                    vec![Line::from(Span::styled(
                        "  No entries match the current filter. Press 'f' to edit.",
                        Style::default().fg(THEME.dim),
                    ))]
                } else {
                    vec![Line::from(Span::styled(
                        "  All sources filtered out. Press 'a' to show all.",
                        Style::default().fg(THEME.dim),
                    ))]
                }
            } else {
                vec![Line::from(Span::styled(
                    "  No task running. Press q to quit.",
                    Style::default().fg(THEME.dim),
                ))]
            }
        } else {
            // Convert filtered entries to a contiguous slice for viewport
            let owned_entries: Vec<LogEntry> = visible_entries.into_iter().cloned().collect();

            // Build the per-frame source labels map from the latest graph
            // snapshot (or empty if no engine yet — fallback `[N] tN`).
            let source_labels = state
                .engine
                .as_ref()
                .map(|h| h.graph.borrow().source_labels())
                .unwrap_or_default();

            // Use the viewport to compute which entries are visible
            let vp_layout = viewport::layout(
                &state.scroll,
                &owned_entries,
                log_height,
                log_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
                Some(&state.field_stats),
                state.show_fields,
                &source_labels,
            );

            // Build a line buffer for the entire viewport, initialized to empty
            let mut line_buffer: Vec<Line<'static>> =
                (0..log_height).map(|_| Line::from("")).collect();

            // Determine if search highlighting is needed
            let search_pattern = if state.search.active {
                Some(state.search.pattern.clone())
            } else {
                None
            };
            let current_search_entry = state.search.current_match_index();

            // Place rendered entries into the buffer at their Y positions
            let cursor_style = Style::default().bg(THEME.selection_bg);
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
            let mode_text = if state.picker_open {
                "PICKER"
            } else if state.quit_confirm {
                "QUIT?"
            } else {
                match state.mode {
                    AppMode::Normal | AppMode::Help => "NORMAL",
                    AppMode::FilterInput => "FILTER",
                    AppMode::SearchInput => "SEARCH",
                    AppMode::EntryDetail => "DETAIL",
                    AppMode::ProcessDetail => "PROCESS",
                    AppMode::CopyMenu => "COPY",
                    AppMode::KillMenu => "KILL",
                }
            };

            let focus_text = if state.sidebar.focused {
                "SIDEBAR"
            } else {
                mode_text
            };

            // Build status line with task info
            let mut spans = vec![
                Span::styled(" runme ", Style::default().fg(Color::Black).bg(THEME.accent)),
                Span::raw(" "),
                Span::styled(
                    format!(" {} ", focus_text),
                    Style::default().fg(Color::Black).bg(THEME.dim),
                ),
            ];

            // Add task name from the current task definition (last launched).
            if let Some(task) = state.current_task {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!(" {} ", task.name),
                    Style::default().fg(Color::White).bg(THEME.dim),
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
                Style::default().fg(THEME.dim),
            ));

            // Source filter indicator (focus + manual hides composed).
            if !state.focus_filter.is_empty() || !state.hidden_sources.is_empty() {
                let hidden_count = state.sidebar_entries.iter().filter(|e| !e.visible).count();
                if hidden_count > 0 {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        format!(" {} hidden ", hidden_count),
                        Style::default().fg(THEME.level_warn),
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
                            Style::default().fg(THEME.dim),
                        ));
                    }
                    ScrollState::Pinned { cursor, .. } => {
                        let new_count = new_entries_since_pin(&state.scroll, visible_count);
                        let pos_text = if new_count > 0 {
                            format!(" {} / {} (+{} new) ", cursor + 1, visible_count, new_count)
                        } else {
                            format!(" {} / {} ", cursor + 1, visible_count)
                        };
                        spans.push(Span::styled(pos_text, Style::default().fg(THEME.dim)));
                    }
                }
            }

            let status_line = Line::from(spans);

            let status_bar = Paragraph::new(status_line)
                .style(Style::default().bg(THEME.dim).fg(Color::White));

            frame.render_widget(status_bar, status_bar_area);
        }

        // -- Help overlay --
        if state.mode == AppMode::Help {
            render_help_overlay(frame, area);
        }

        // -- Copy menu overlay --
        if state.mode == AppMode::CopyMenu {
            render_copy_menu(frame, area);
        }

        // -- Kill menu overlay (design decision 4) --
        if state.mode == AppMode::KillMenu {
            render_kill_menu(frame, area);
        }

        // -- Entry detail overlay --
        if state.mode == AppMode::EntryDetail {
            render_entry_detail(frame, area, state);
        }

        // -- Process detail overlay --
        if state.mode == AppMode::ProcessDetail {
            render_process_detail(frame, area, state);
        }

        // -- Task picker overlay (decisions 1 + 8) --
        if state.picker_open
            && let Some(ref mut picker_state) = state.picker
        {
            render_picker_overlay(frame, area, picker_state);
        }

        // -- Quit-confirmation overlay (decision 7) --
        if state.quit_confirm {
            render_quit_confirm(frame, area);
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

    let section = Style::default().fg(THEME.level_warn);
    let desc = Style::default().fg(THEME.dim);

    let help_text = vec![
        Line::from(Span::styled(
            "Keyboard Shortcuts",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![Span::styled("Navigation", section)]),
        Line::from(vec![
            Span::raw("  j/k    "),
            Span::styled("Move cursor down/up", desc),
        ]),
        Line::from(vec![
            Span::raw("  [/]    "),
            Span::styled("Page up/down", desc),
        ]),
        Line::from(vec![
            Span::raw("  g/G    "),
            Span::styled("Jump to top / bottom (tail)", desc),
        ]),
        Line::from(vec![
            Span::raw("  Enter  "),
            Span::styled("Open entry detail view", desc),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("Display", section)]),
        Line::from(vec![
            Span::raw("  v      "),
            Span::styled("Toggle preview/raw mode", desc),
        ]),
        Line::from(vec![
            Span::raw("  w      "),
            Span::styled("Toggle wrap/truncate", desc),
        ]),
        Line::from(vec![
            Span::raw("  d      "),
            Span::styled("Toggle field details", desc),
        ]),
        Line::from(vec![
            Span::raw("  \\      "),
            Span::styled("Toggle sidebar visibility", desc),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("Filter & Search", section)]),
        Line::from(vec![
            Span::raw("  f      "),
            Span::styled("Open filter bar (Enter confirm, Esc cancel)", desc),
        ]),
        Line::from(vec![
            Span::raw("  /      "),
            Span::styled("Open search (Enter confirm, Esc cancel)", desc),
        ]),
        Line::from(vec![
            Span::raw("  n/N    "),
            Span::styled("Next/previous search match", desc),
        ]),
        Line::from(vec![
            Span::raw("  Up/Dn  "),
            Span::styled("Cycle filter history (in filter input)", desc),
        ]),
        Line::from(vec![
            Span::raw("  1-9    "),
            Span::styled("Toggle source N visibility", desc),
        ]),
        Line::from(vec![
            Span::raw("  a      "),
            Span::styled("Show all sources", desc),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("Sidebar (Tab to focus)", section)]),
        Line::from(vec![
            Span::raw("  Tab    "),
            Span::styled("Toggle sidebar focus", desc),
        ]),
        Line::from(vec![
            Span::raw("  Enter  "),
            Span::styled("Process detail / toggle source visibility", desc),
        ]),
        Line::from(vec![
            Span::raw("  s      "),
            Span::styled("Stop selected process (SIGTERM)", desc),
        ]),
        Line::from(vec![
            Span::raw("  S      "),
            Span::styled("Send SIGHUP to selected process", desc),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("Copy / Export", section)]),
        Line::from(vec![
            Span::raw("  y      "),
            Span::styled("Copy selected entry", desc),
        ]),
        Line::from(vec![
            Span::raw("  c      "),
            Span::styled("Copy menu (viewport/stream/all)", desc),
        ]),
        Line::from(vec![
            Span::raw("  e      "),
            Span::styled("Export visible log to file", desc),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  t      "),
            Span::styled("Open task picker (re-entrant)", desc),
        ]),
        Line::from(vec![
            Span::raw("  r      "),
            Span::styled("Restart task", desc),
        ]),
        Line::from(vec![
            Span::raw("  k      "),
            Span::styled("Kill menu (kk=TERM, k9=KILL, ka=all)", desc),
        ]),
        Line::from(vec![
            Span::raw("  q      "),
            Span::styled("Quit (prompts if tasks running)", desc),
        ]),
        Line::from(vec![
            Span::raw("  ?      "),
            Span::styled("Toggle this help", desc),
        ]),
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
        .border_style(Style::default().fg(THEME.accent))
        .title(Span::styled(" Help ", Style::default().fg(THEME.accent)));

    let help_paragraph = Paragraph::new(help_text)
        .block(help_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(help_paragraph, popup_area);
}

/// Render the copy menu overlay.
fn render_copy_menu(frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
    use ratatui::widgets::{Borders, Clear, Wrap};

    let desc = Style::default().fg(THEME.dim);
    let menu_text = vec![
        Line::from(Span::styled(
            "Copy to Clipboard",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  v  "),
            Span::styled("Viewport (on-screen entries)", desc),
        ]),
        Line::from(vec![
            Span::raw("  s  "),
            Span::styled("Stream (selected source)", desc),
        ]),
        Line::from(vec![
            Span::raw("  a  "),
            Span::styled("All (matching filter)", desc),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Esc to cancel", desc)),
    ];

    let menu_height = (menu_text.len() + 2) as u16;
    let menu_width = 40u16;

    let x = area.width.saturating_sub(menu_width) / 2;
    let y = area.height.saturating_sub(menu_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        menu_width.min(area.width),
        menu_height.min(area.height),
    );

    frame.render_widget(Clear, popup_area);

    let menu_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .title(Span::styled(" Copy ", Style::default().fg(THEME.accent)));

    let menu_paragraph = Paragraph::new(menu_text)
        .block(menu_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(menu_paragraph, popup_area);
}

/// Render the kill menu overlay (design decision 4). Mirrors the copy
/// menu visually — small centered popup listing the chord follow-ups.
fn render_kill_menu(frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
    use ratatui::widgets::{Borders, Clear, Wrap};

    let desc = Style::default().fg(THEME.dim);
    let menu_text = vec![
        Line::from(Span::styled(
            "Kill Task",
            Style::default()
                .fg(THEME.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  k  "),
            Span::styled("SIGTERM the focused task", desc),
        ]),
        Line::from(vec![
            Span::raw("  9  "),
            Span::styled("SIGKILL the focused task", desc),
        ]),
        Line::from(vec![
            Span::raw("  a  "),
            Span::styled("Terminate all tasks", desc),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Esc to cancel", desc)),
    ];

    let menu_height = (menu_text.len() + 2) as u16;
    let menu_width = 40u16;

    let x = area.width.saturating_sub(menu_width) / 2;
    let y = area.height.saturating_sub(menu_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        menu_width.min(area.width),
        menu_height.min(area.height),
    );

    frame.render_widget(Clear, popup_area);

    let menu_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .title(Span::styled(" Kill ", Style::default().fg(THEME.accent)));

    let menu_paragraph = Paragraph::new(menu_text)
        .block(menu_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(menu_paragraph, popup_area);
}

/// Render the task picker as a centered overlay covering most of the
/// screen. The Normal-mode shell (sidebar, log pane, status bar) stays
/// visible behind it (decisions 1 + 8).
fn render_picker_overlay(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    picker: &mut super::picker::PickerState,
) {
    use ratatui::widgets::Clear;

    // Picker takes ~80% of width and height, capped at sensible bounds,
    // centered. Leaves the shell visible at the edges.
    let picker_width = ((area.width as u32 * 80 / 100) as u16).clamp(40, 100);
    let picker_height = ((area.height as u32 * 80 / 100) as u16).clamp(10, 40);
    let picker_width = picker_width.min(area.width);
    let picker_height = picker_height.min(area.height);

    let x = area.width.saturating_sub(picker_width) / 2;
    let y = area.height.saturating_sub(picker_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        picker_width,
        picker_height,
    );

    frame.render_widget(Clear, popup_area);
    picker::render_picker(frame, popup_area, picker);
}

/// Render the quit-confirmation modal (decision 7). Only shown when
/// `q` is pressed while running tasks exist.
fn render_quit_confirm(frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
    use ratatui::widgets::{Borders, Clear, Wrap};

    let desc = Style::default().fg(THEME.dim);
    let modal_text = vec![
        Line::from(Span::styled(
            "Tasks still running.",
            Style::default()
                .fg(THEME.level_warn)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("[enter] quit  [esc] cancel", desc)),
    ];

    let modal_height = (modal_text.len() + 2) as u16;
    let modal_width = 36u16;

    let x = area.width.saturating_sub(modal_width) / 2;
    let y = area.height.saturating_sub(modal_height) / 2;
    let popup_area = ratatui::layout::Rect::new(
        area.x + x,
        area.y + y,
        modal_width.min(area.width),
        modal_height.min(area.height),
    );

    frame.render_widget(Clear, popup_area);

    let modal_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.level_warn))
        .title(Span::styled(
            " Quit ",
            Style::default().fg(THEME.level_warn),
        ));

    let modal_paragraph = Paragraph::new(modal_text)
        .block(modal_block)
        .wrap(Wrap { trim: false });

    frame.render_widget(modal_paragraph, popup_area);
}

/// Render the entry detail overlay showing all fields of the focused log entry.
fn render_entry_detail(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
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
    let label = Style::default().fg(THEME.accent);
    let mut detail_lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled("timestamp: ", label),
            Span::raw(entry.display_timestamp()),
        ]),
        Line::from(vec![
            Span::styled("level:     ", label),
            Span::raw(entry.level.clone().unwrap_or_else(|| "---".to_string())),
        ]),
        Line::from(vec![
            Span::styled("source:    ", label),
            Span::raw(entry.source.to_string()),
        ]),
        Line::from(vec![
            Span::styled("message:   ", label),
            Span::raw(
                entry
                    .message
                    .clone()
                    .unwrap_or_else(|| "(none)".to_string()),
            ),
        ]),
    ];

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
                    Style::default().fg(THEME.level_warn),
                ),
                Span::raw(value_str),
            ]));
        }
    }

    // Raw text section
    detail_lines.push(Line::from(""));
    detail_lines.push(Line::from(Span::styled(
        "--- raw ---",
        Style::default().fg(THEME.dim),
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
    let visible_lines: Vec<Line<'static>> = detail_lines.into_iter().skip(scroll_offset).collect();

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .title(Span::styled(
            " Entry Detail (j/k scroll, y copy, Esc close) ",
            Style::default().fg(THEME.accent),
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
    let label = Style::default().fg(THEME.accent);
    let mut detail_lines: Vec<Line<'static>> = Vec::new();

    detail_lines.push(Line::from(vec![
        Span::styled("Command:  ", label),
        Span::raw(entry.name.clone()),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("Source:   ", label),
        Span::raw(entry.source.to_string()),
    ]));

    detail_lines.push(Line::from(vec![
        Span::styled("Status:   ", label),
        Span::styled(
            entry.status_tag.clone(),
            Style::default().fg(entry.status_color),
        ),
    ]));

    // Look up PID/PGID via the engine's graph snapshot, matched by the
    // sidebar entry's source TaskId (which is the process id).
    if let Some(handle) = state.engine.as_ref() {
        let snapshot = handle.graph.borrow().clone();
        'find_proc: for node in snapshot.tasks.values() {
            for proc in &node.processes {
                if proc.id == entry.source {
                    if let Some(pid) = proc.pid {
                        detail_lines.push(Line::from(vec![
                            Span::styled("PID:      ", label),
                            Span::raw(pid.to_string()),
                        ]));
                    }
                    if let Some(pgid) = proc.pgid {
                        detail_lines.push(Line::from(vec![
                            Span::styled("PGID:     ", label),
                            Span::raw(pgid.to_string()),
                        ]));
                    }
                    break 'find_proc;
                }
            }
        }
    }

    // Listening ports
    detail_lines.push(Line::from(""));
    if let Some(ref sockets) = state.process_detail_sockets {
        detail_lines.push(Line::from(vec![
            Span::styled("Ports:   ", label),
            Span::raw(sockets.clone()),
        ]));
    } else {
        detail_lines.push(Line::from(vec![
            Span::styled("Ports:   ", label),
            Span::styled("scanning...", Style::default().fg(THEME.level_warn)),
        ]));
    }

    // Controls hint at bottom
    detail_lines.push(Line::from(""));
    detail_lines.push(Line::from(vec![
        Span::styled("s", Style::default().fg(THEME.accent)),
        Span::raw(" stop (SIGTERM)  "),
        Span::styled("S", Style::default().fg(THEME.accent)),
        Span::raw(" SIGHUP  "),
        Span::styled("j/k", Style::default().fg(THEME.accent)),
        Span::raw(" scroll  "),
        Span::styled("Esc", Style::default().fg(THEME.accent)),
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
        state
            .process_detail_scroll
            .min(total_lines.saturating_sub(1))
    } else {
        0
    };
    let visible_lines: Vec<Line<'static>> = detail_lines.into_iter().skip(scroll_offset).collect();

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .title(Span::styled(
            " Process Detail ",
            Style::default().fg(THEME.accent),
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

    let notif_area = ratatui::layout::Rect::new(log_area.x, log_area.y, log_area.width, 1);

    frame.render_widget(Clear, notif_area);
    let line = Line::from(vec![
        Span::styled(" ! ", Style::default().fg(Color::Black).bg(THEME.level_warn)),
        Span::raw(" "),
        Span::styled(text.clone(), Style::default().fg(THEME.level_warn)),
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
    use super::search::{current_match_highlight_style, find_match_ranges, match_highlight_style};

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
                    new_spans.push(Span::styled(text[pos..range.start].to_string(), span.style));
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
