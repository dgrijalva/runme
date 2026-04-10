//! Filter input widget for the TUI.
//!
//! Provides a text input component for entering filter expressions.
//! Renders in the status bar area when active, with inline parse error display.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::theme::THEME;

use crate::log::filter::{self, FilterExpr};

/// State for the filter input widget.
pub struct FilterInputState {
    /// Current text in the input buffer.
    pub text: String,
    /// Cursor position within the text (byte offset, always on a char boundary).
    pub cursor: usize,
    /// The last successfully parsed filter expression.
    /// None if the input is empty or has never been valid.
    pub last_valid_expr: Option<FilterExpr>,
    /// The text that produced `last_valid_expr`, so we can display it.
    pub last_valid_text: String,
    /// Current parse error, if any.
    pub parse_error: Option<String>,
    /// The text/expr that was active before entering filter mode (for cancel/revert).
    pub saved_text: String,
    pub saved_expr: Option<FilterExpr>,
}

impl Default for FilterInputState {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterInputState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            last_valid_expr: None,
            last_valid_text: String::new(),
            parse_error: None,
            saved_text: String::new(),
            saved_expr: None,
        }
    }

    /// Save the current filter state before entering filter input mode.
    /// This allows reverting on Esc.
    pub fn save_current(&mut self) {
        self.saved_text = self.text.clone();
        // We can't clone FilterExpr (it contains Regex), so we re-parse from saved_text.
        self.saved_expr = if self.saved_text.is_empty() {
            None
        } else {
            filter::parse(&self.saved_text).ok()
        };
    }

    /// Revert to the saved state (on Esc).
    pub fn revert(&mut self) {
        self.text = self.saved_text.clone();
        self.cursor = self.text.len();
        self.parse_error = None;
        // Re-parse the saved text to restore last_valid_expr
        if self.text.is_empty() {
            self.last_valid_expr = None;
            self.last_valid_text.clear();
        } else if let Ok(expr) = filter::parse(&self.text) {
            self.last_valid_expr = Some(expr);
            self.last_valid_text = self.text.clone();
        }
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.reparse();
    }

    /// Delete the character before the cursor (Backspace).
    pub fn delete_char_before(&mut self) {
        if self.cursor > 0 {
            // Find the previous char boundary
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.remove(prev);
            self.cursor = prev;
            self.reparse();
        }
    }

    /// Move cursor left by one character.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right by one character.
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Clear the input (Ctrl-u).
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.last_valid_expr = None;
        self.last_valid_text.clear();
        self.parse_error = None;
    }

    /// Re-parse the current text and update the valid expr / error state.
    fn reparse(&mut self) {
        if self.text.is_empty() {
            self.last_valid_expr = None;
            self.last_valid_text.clear();
            self.parse_error = None;
            return;
        }

        match filter::parse(&self.text) {
            Ok(expr) => {
                self.last_valid_expr = Some(expr);
                self.last_valid_text = self.text.clone();
                self.parse_error = None;
            }
            Err(e) => {
                // Keep last_valid_expr as-is; show the error
                self.parse_error = Some(e);
            }
        }
    }

    /// Get the current active filter expression (the last valid one).
    pub fn active_expr(&self) -> Option<&FilterExpr> {
        self.last_valid_expr.as_ref()
    }

    /// Whether there is an active filter (either valid expr or text present).
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

/// Render the filter input bar into the given area.
pub fn render_filter_input(frame: &mut Frame, area: Rect, filter_state: &FilterInputState) {
    let mut spans: Vec<Span> = Vec::new();

    // Prefix
    spans.push(Span::styled(" filter: ", Style::default().fg(THEME.accent)));

    if filter_state.text.is_empty() {
        // Placeholder
        spans.push(Span::styled(
            "level:error AND source:api ...",
            Style::default().fg(THEME.dim),
        ));
    } else {
        // Split text at cursor position for rendering
        let before = &filter_state.text[..filter_state.cursor];
        let after = &filter_state.text[filter_state.cursor..];

        spans.push(Span::styled(
            before.to_string(),
            Style::default().fg(Color::White),
        ));

        // Cursor character (highlight the char at cursor, or a space if at end)
        if after.is_empty() {
            spans.push(Span::styled(
                " ",
                Style::default().fg(Color::Black).bg(Color::White),
            ));
        } else {
            let cursor_char = &after[..after.chars().next().unwrap().len_utf8()];
            let rest = &after[cursor_char.len()..];
            spans.push(Span::styled(
                cursor_char.to_string(),
                Style::default().fg(Color::Black).bg(Color::White),
            ));
            spans.push(Span::styled(
                rest.to_string(),
                Style::default().fg(Color::White),
            ));
        }
    }

    // Parse error display
    if let Some(ref err) = filter_state.parse_error {
        spans.push(Span::styled(
            format!("  {}", err),
            Style::default().fg(THEME.level_error),
        ));
    }

    let line = Line::from(spans);
    let paragraph =
        Paragraph::new(line).style(Style::default().bg(THEME.dim).fg(Color::White));
    frame.render_widget(paragraph, area);
}

/// Render the filter indicator in the status bar when not in filter mode.
/// Returns spans to add to the status bar, or empty vec if no filter active.
pub fn filter_status_spans(filter_state: &FilterInputState) -> Vec<Span<'static>> {
    match filter_state.status_display() {
        Some(text) => {
            vec![
                Span::raw(" "),
                Span::styled(
                    format!(" filter: {} ", text),
                    Style::default().fg(THEME.accent),
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
        assert!(state.text.is_empty());
        assert_eq!(state.cursor, 0);
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
        assert_eq!(state.text, "hi");
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut state = FilterInputState::new();
        state.insert_char('a');
        state.insert_char('b');
        state.insert_char('c');
        state.delete_char_before();
        assert_eq!(state.text, "ab");
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut state = FilterInputState::new();
        state.delete_char_before();
        assert!(state.text.is_empty());
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn move_left_right() {
        let mut state = FilterInputState::new();
        state.insert_char('a');
        state.insert_char('b');
        state.insert_char('c');
        assert_eq!(state.cursor, 3);

        state.move_left();
        assert_eq!(state.cursor, 2);
        state.move_left();
        assert_eq!(state.cursor, 1);
        state.move_left();
        assert_eq!(state.cursor, 0);
        state.move_left(); // at start, no-op
        assert_eq!(state.cursor, 0);

        state.move_right();
        assert_eq!(state.cursor, 1);
        state.move_right();
        state.move_right();
        assert_eq!(state.cursor, 3);
        state.move_right(); // at end, no-op
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn clear_resets_everything() {
        let mut state = FilterInputState::new();
        state.insert_char('x');
        state.insert_char('y');
        state.clear();
        assert!(state.text.is_empty());
        assert_eq!(state.cursor, 0);
        assert!(state.last_valid_expr.is_none());
        assert!(state.parse_error.is_none());
    }

    #[test]
    fn valid_expr_is_parsed() {
        let mut state = FilterInputState::new();
        // Type a valid filter: "error"
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
        // Type a valid filter
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        assert!(state.last_valid_expr.is_some());
        assert!(state.parse_error.is_none());

        // Now type something that creates a parse error.
        // "error AND OR" is invalid because OR needs a left operand after AND.
        for ch in " AND OR".chars() {
            state.insert_char(ch);
        }
        // The last valid expr should still be active
        assert!(state.has_active_filter());
        // There should be a parse error
        assert!(state.parse_error.is_some());
    }

    #[test]
    fn save_and_revert() {
        let mut state = FilterInputState::new();
        for ch in "level:error".chars() {
            state.insert_char(ch);
        }
        state.save_current();

        // Modify the text
        state.clear();
        for ch in "new filter".chars() {
            state.insert_char(ch);
        }
        assert_eq!(state.text, "new filter");

        // Revert
        state.revert();
        assert_eq!(state.text, "level:error");
        assert_eq!(state.cursor, state.text.len());
    }

    #[test]
    fn insert_in_middle() {
        let mut state = FilterInputState::new();
        state.insert_char('a');
        state.insert_char('c');
        state.move_left(); // cursor between 'a' and 'c'
        state.insert_char('b');
        assert_eq!(state.text, "abc");
        assert_eq!(state.cursor, 2); // after 'b'
    }

    #[test]
    fn delete_in_middle() {
        let mut state = FilterInputState::new();
        state.insert_char('a');
        state.insert_char('b');
        state.insert_char('c');
        state.move_left(); // cursor after 'b', before 'c'
        state.delete_char_before(); // delete 'b'
        assert_eq!(state.text, "ac");
        assert_eq!(state.cursor, 1); // after 'a'
    }

    #[test]
    fn field_filter_parses() {
        let mut state = FilterInputState::new();
        for ch in "level:error".chars() {
            state.insert_char(ch);
        }
        assert!(state.last_valid_expr.is_some());
        assert!(state.parse_error.is_none());
    }

    #[test]
    fn complex_filter_parses() {
        let mut state = FilterInputState::new();
        for ch in "level:error AND source:api".chars() {
            state.insert_char(ch);
        }
        assert!(state.last_valid_expr.is_some());
        assert!(state.parse_error.is_none());
    }
}
