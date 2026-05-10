//! Filter input state for the TUI.
//!
//! Wraps the shared `TextInput` control and keeps a live-parsed filter
//! expression in sync as the user types. Rendering is handled by the
//! shared chrome in `text_input::render_chrome`.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Span,
};

use crate::log::filter::{self, FilterExpr};
use crate::theme::THEME;
use crate::tui::text_input::{ChromeOptions, TextInput, render_chrome};

/// Hint string shown in the bottom border of the filter input box.
const FILTER_HINTS: &str = "[enter] save  [esc] cancel";

/// Placeholder shown in the input when empty.
const FILTER_PLACEHOLDER: &str = "level:error AND source:api ...";

/// State for the filter input widget.
pub struct FilterInputState {
    /// Shared text-input control: text, cursor, history, virtual slot.
    pub input: TextInput,
    /// The last successfully parsed filter expression.
    /// None if the input is empty or has never been valid.
    pub last_valid_expr: Option<FilterExpr>,
    /// The text that produced `last_valid_expr`, so we can display it.
    pub last_valid_text: String,
    /// Current parse error, if any.
    pub parse_error: Option<String>,
}

impl Default for FilterInputState {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterInputState {
    pub fn new() -> Self {
        Self {
            input: TextInput::new(),
            last_valid_expr: None,
            last_valid_text: String::new(),
            parse_error: None,
        }
    }

    /// Save the current filter state before entering filter input mode.
    /// Snapshots for revert and resets the in-session history nav state.
    pub fn save_current(&mut self) {
        self.input.save_current();
    }

    /// Revert to the saved state (on Esc). Re-parses to restore
    /// `last_valid_expr` from the restored text.
    pub fn revert(&mut self) {
        self.input.revert();
        self.reparse();
    }

    /// Commit the current text to history (with MRU dedup) and update
    /// the active filter expression. Empty text clears the active filter.
    pub fn commit(&mut self) {
        self.input.commit();
        self.reparse();
    }

    pub fn insert_char(&mut self, ch: char) {
        self.input.insert_char(ch);
        self.reparse();
    }

    pub fn delete_char_before(&mut self) {
        self.input.delete_char_before();
        self.reparse();
    }

    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    pub fn set_text(&mut self, text: String) {
        self.input.set_text(text);
        self.reparse();
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.last_valid_expr = None;
        self.last_valid_text.clear();
        self.parse_error = None;
    }

    pub fn history_up(&mut self) {
        self.input.history_up();
        self.reparse();
    }

    pub fn history_down(&mut self) {
        self.input.history_down();
        self.reparse();
    }

    /// Re-parse the current text and update the valid expr / error state.
    fn reparse(&mut self) {
        if self.input.text.is_empty() {
            self.last_valid_expr = None;
            self.last_valid_text.clear();
            self.parse_error = None;
            return;
        }

        match filter::parse(&self.input.text) {
            Ok(expr) => {
                self.last_valid_expr = Some(expr);
                self.last_valid_text = self.input.text.clone();
                self.parse_error = None;
            }
            Err(e) => {
                self.parse_error = Some(e);
            }
        }
    }

    pub fn active_expr(&self) -> Option<&FilterExpr> {
        self.last_valid_expr.as_ref()
    }

    pub fn has_active_filter(&self) -> bool {
        self.last_valid_expr.is_some()
    }

    /// Get display text for the status bar when NOT in filter input mode.
    /// Returns None if no filter is active.
    pub fn status_display(&self) -> Option<String> {
        if self.last_valid_text.is_empty() {
            None
        } else {
            Some(self.last_valid_text.clone())
        }
    }
}

/// Render the filter input bar (bordered chrome with title + hints).
pub fn render_filter_input(frame: &mut Frame, area: Rect, filter_state: &FilterInputState) {
    let trailing = filter_state.parse_error.as_ref().map(|err| {
        Span::styled(err.clone(), Style::default().fg(THEME.level_error))
    });

    render_chrome(
        frame,
        area,
        &filter_state.input,
        ChromeOptions {
            title: "filter",
            hints: FILTER_HINTS,
            trailing,
            placeholder: FILTER_PLACEHOLDER,
        },
    );
}

/// Render the active-filter chip in the status bar when not in filter mode.
/// Returns spans to add to the status bar, or empty vec if no filter active.
pub fn filter_status_spans(filter_state: &FilterInputState) -> Vec<Span<'static>> {
    match filter_state.status_display() {
        Some(text) => {
            vec![
                Span::raw(" "),
                Span::styled(
                    format!(" filter: {} ", text),
                    Style::default().fg(Color::Black).bg(THEME.accent),
                ),
            ]
        }
        None => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_empty() {
        let state = FilterInputState::new();
        assert!(state.input.text.is_empty());
        assert_eq!(state.input.cursor, 0);
        assert!(state.last_valid_expr.is_none());
        assert!(state.parse_error.is_none());
        assert!(!state.has_active_filter());
        assert!(state.status_display().is_none());
    }

    #[test]
    fn insert_char_updates_text_and_cursor() {
        let mut state = FilterInputState::new();
        state.insert_char('h');
        state.insert_char('i');
        assert_eq!(state.input.text, "hi");
        assert_eq!(state.input.cursor, 2);
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut state = FilterInputState::new();
        state.insert_char('a');
        state.insert_char('b');
        state.insert_char('c');
        state.delete_char_before();
        assert_eq!(state.input.text, "ab");
        assert_eq!(state.input.cursor, 2);
    }

    #[test]
    fn clear_resets_everything() {
        let mut state = FilterInputState::new();
        state.insert_char('x');
        state.insert_char('y');
        state.clear();
        assert!(state.input.text.is_empty());
        assert_eq!(state.input.cursor, 0);
        assert!(state.last_valid_expr.is_none());
        assert!(state.parse_error.is_none());
    }

    #[test]
    fn valid_expr_is_parsed() {
        let mut state = FilterInputState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        assert!(state.last_valid_expr.is_some());
        assert!(state.parse_error.is_none());
        assert!(state.has_active_filter());
        assert_eq!(state.status_display(), Some("error".to_string()));
    }

    #[test]
    fn parse_error_keeps_last_valid() {
        let mut state = FilterInputState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        assert!(state.last_valid_expr.is_some());

        for ch in " AND OR".chars() {
            state.insert_char(ch);
        }
        assert!(state.has_active_filter());
        assert!(state.parse_error.is_some());
    }

    #[test]
    fn save_and_revert() {
        let mut state = FilterInputState::new();
        for ch in "level:error".chars() {
            state.insert_char(ch);
        }
        state.save_current();

        state.clear();
        for ch in "new filter".chars() {
            state.insert_char(ch);
        }
        assert_eq!(state.input.text, "new filter");

        state.revert();
        assert_eq!(state.input.text, "level:error");
        assert_eq!(state.input.cursor, state.input.text.len());
    }

    #[test]
    fn commit_pushes_to_history() {
        let mut state = FilterInputState::new();
        state.save_current();
        for ch in "level:error".chars() {
            state.insert_char(ch);
        }
        state.commit();
        assert_eq!(state.input.history, vec!["level:error".to_string()]);
    }

    #[test]
    fn history_navigation_with_virtual_slot() {
        let mut state = FilterInputState::new();
        state.save_current();
        for ch in "foo".chars() {
            state.insert_char(ch);
        }
        state.commit();
        // Reopen the panel — text persists as the active filter
        state.save_current();
        // Down should clear (virtual → blank)
        state.history_down();
        assert!(state.input.text.is_empty());
        assert!(state.last_valid_expr.is_none());
        // Up returns to virtual='foo'
        state.history_up();
        assert_eq!(state.input.text, "foo");
        assert!(state.has_active_filter());
    }
}
