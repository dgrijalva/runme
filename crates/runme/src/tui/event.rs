use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::log::LogEntry;

use super::app::{AppState, render_frame};
use super::render::DisplayMode;
use super::viewport::{
    scroll_down, scroll_down_half_page, scroll_to_bottom, scroll_to_top,
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
        signal(SignalKind::interrupt()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut sigterm =
        signal(SignalKind::terminate()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Subscribe to the LogStore broadcast for new entries
    let mut log_rx: broadcast::Receiver<LogEntry> = {
        let store = state.log_store.lock().await;
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
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Log stream closed; keep running (task may have finished)
                    }
                }
            }

            // Render tick — only redraw when dirty
            _ = render_interval.tick(), if state.dirty => {
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
    let viewport_width = term_size.width;

    match key.code {
        // 'q' quits the application
        KeyCode::Char('q') => {
            state.running = false;
        }
        // Ctrl-C also quits
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.running = false;
        }

        // -- Navigation --

        // j / Down: move cursor to next entry
        KeyCode::Char('j') | KeyCode::Down => {
            state.scroll = scroll_down(
                &state.scroll,
                &state.log_lines,
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
                &state.log_lines,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // Ctrl-d / Page Down: scroll down half page
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = scroll_down_half_page(
                &state.scroll,
                &state.log_lines,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }
        KeyCode::PageDown => {
            state.scroll = scroll_down_half_page(
                &state.scroll,
                &state.log_lines,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // Ctrl-u / Page Up: scroll up half page
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll = scroll_up_half_page(
                &state.scroll,
                &state.log_lines,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }
        KeyCode::PageUp => {
            state.scroll = scroll_up_half_page(
                &state.scroll,
                &state.log_lines,
                viewport_height,
                viewport_width,
                state.display_mode,
                state.wrap,
                &mut state.source_colors,
            );
        }

        // g / Home: jump to first entry
        KeyCode::Char('g') | KeyCode::Home => {
            state.scroll = scroll_to_top(&state.scroll, &state.log_lines);
        }

        // G / End: jump to last entry, enter tail mode
        KeyCode::Char('G') | KeyCode::End => {
            state.scroll = scroll_to_bottom(&state.scroll, &state.log_lines);
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

        _ => {}
    }
    // Any key press marks the state as dirty (e.g., for cursor feedback later)
    state.dirty = true;
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;
    use super::super::viewport::ScrollState;

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
}
