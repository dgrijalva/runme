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
                        handle_event(event, state);
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
fn handle_event(event: Event, state: &mut AppState) {
    match event {
        Event::Key(key_event) => handle_key(key_event, state),
        Event::Resize(_, _) => {
            state.dirty = true;
        }
        _ => {}
    }
}

/// Handle a keyboard event.
fn handle_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        // 'q' quits the application
        KeyCode::Char('q') => {
            state.running = false;
        }
        // Ctrl-C also quits
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.running = false;
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
        handle_key(make_key_event(KeyCode::Char('q'), KeyModifiers::NONE), &mut state);
        assert!(!state.running);
    }

    #[test]
    fn ctrl_c_sets_running_false() {
        let mut state = AppState::new();
        assert!(state.running);
        handle_key(
            make_key_event(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert!(!state.running);
    }

    #[test]
    fn resize_sets_dirty() {
        let mut state = AppState::new();
        state.dirty = false;
        handle_event(Event::Resize(80, 24), &mut state);
        assert!(state.dirty);
    }

    #[test]
    fn other_key_sets_dirty() {
        let mut state = AppState::new();
        state.dirty = false;
        handle_key(make_key_event(KeyCode::Char('j'), KeyModifiers::NONE), &mut state);
        assert!(state.dirty);
        assert!(state.running); // should still be running
    }
}
