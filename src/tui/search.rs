//! Search input and match tracking for the TUI.
//!
//! Provides a text input component for entering search patterns,
//! match tracking with navigation (n/N), and search highlighting.

use std::ops::Range;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::theme::THEME;

/// State for the search feature.
pub struct SearchState {
    /// Current text in the search input buffer.
    pub text: String,
    /// Cursor position within the text (byte offset, always on a char boundary).
    pub cursor: usize,
    /// Whether search is active (pattern confirmed and highlighting).
    pub active: bool,
    /// The confirmed search pattern (set on Enter).
    pub pattern: String,
    /// Indices into the visible (filtered) entry list that match the pattern.
    pub match_indices: Vec<usize>,
    /// Current position within match_indices (which match n/N is focused on).
    pub current_match: usize,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            active: false,
            pattern: String::new(),
            match_indices: Vec::new(),
            current_match: 0,
        }
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Delete the character before the cursor (Backspace).
    pub fn delete_char_before(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.remove(prev);
            self.cursor = prev;
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

    /// Clear the search input (Ctrl-u).
    pub fn clear_input(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Confirm the search: set pattern, mark active.
    /// The caller must then call `scan_matches` to populate match_indices.
    pub fn confirm(&mut self) {
        self.pattern = self.text.clone();
        if self.pattern.is_empty() {
            self.active = false;
            self.match_indices.clear();
            self.current_match = 0;
        } else {
            self.active = true;
            // match_indices will be populated by scan_matches
        }
    }

    /// Cancel the search: clear everything.
    pub fn cancel(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.active = false;
        self.pattern.clear();
        self.match_indices.clear();
        self.current_match = 0;
    }

    /// Scan visible entries for the current pattern (case-insensitive substring).
    /// `visible_texts` should be an iterator of (visible_index, text) pairs.
    pub fn scan_matches<'a>(&mut self, visible_texts: impl Iterator<Item = (usize, &'a str)>) {
        self.match_indices.clear();
        if self.pattern.is_empty() {
            self.current_match = 0;
            return;
        }
        let pattern_lower = self.pattern.to_lowercase();
        for (idx, text) in visible_texts {
            if text.to_lowercase().contains(&pattern_lower) {
                self.match_indices.push(idx);
            }
        }
        // Clamp current_match
        if self.match_indices.is_empty() {
            self.current_match = 0;
        } else {
            self.current_match = self.current_match.min(self.match_indices.len() - 1);
        }
    }

    /// Check a single new entry against the active search pattern.
    /// If it matches, add its visible index to the match list.
    pub fn check_new_entry(&mut self, visible_index: usize, text: &str) {
        if !self.active || self.pattern.is_empty() {
            return;
        }
        let pattern_lower = self.pattern.to_lowercase();
        if text.to_lowercase().contains(&pattern_lower) {
            self.match_indices.push(visible_index);
        }
    }

    /// Jump to the next match. Returns the visible entry index to navigate to,
    /// or None if no matches.
    pub fn next_match(&mut self) -> Option<usize> {
        if self.match_indices.is_empty() {
            return None;
        }
        self.current_match = (self.current_match + 1) % self.match_indices.len();
        Some(self.match_indices[self.current_match])
    }

    /// Jump to the previous match. Returns the visible entry index to navigate to,
    /// or None if no matches.
    pub fn prev_match(&mut self) -> Option<usize> {
        if self.match_indices.is_empty() {
            return None;
        }
        if self.current_match == 0 {
            self.current_match = self.match_indices.len() - 1;
        } else {
            self.current_match -= 1;
        }
        Some(self.match_indices[self.current_match])
    }

    /// Get the visible entry index of the current match, if any.
    pub fn current_match_index(&self) -> Option<usize> {
        if self.match_indices.is_empty() {
            None
        } else {
            Some(self.match_indices[self.current_match])
        }
    }

    /// Find the nearest match at or after the given visible index and set
    /// current_match to it. Used when confirming search to jump to the first
    /// match near the current cursor position.
    pub fn jump_to_nearest(&mut self, visible_index: usize) -> Option<usize> {
        if self.match_indices.is_empty() {
            return None;
        }
        // Find the first match at or after visible_index
        for (i, &idx) in self.match_indices.iter().enumerate() {
            if idx >= visible_index {
                self.current_match = i;
                return Some(idx);
            }
        }
        // Wrap around to the first match
        self.current_match = 0;
        Some(self.match_indices[0])
    }

    /// Total number of matches.
    pub fn match_count(&self) -> usize {
        self.match_indices.len()
    }

    /// The 1-based position of the current match (for display).
    pub fn current_match_display(&self) -> usize {
        self.current_match + 1
    }
}

/// Find all byte ranges where `pattern` matches (case-insensitive) within `text`.
pub fn find_match_ranges(text: &str, pattern: &str) -> Vec<Range<usize>> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let text_lower = text.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(pos) = text_lower[start..].find(&pattern_lower) {
        let abs = start + pos;
        ranges.push(abs..abs + pattern_lower.len());
        start = abs + pattern_lower.len();
    }
    ranges
}

/// Render the search input bar into the given area.
pub fn render_search_input(frame: &mut Frame, area: Rect, search_state: &SearchState) {
    let mut spans: Vec<Span> = Vec::new();

    // Prefix
    spans.push(Span::styled(" /", Style::default().fg(THEME.level_warn)));

    if search_state.text.is_empty() {
        // Placeholder
        spans.push(Span::styled("search...", Style::default().fg(THEME.dim)));
    } else {
        // Split text at cursor position for rendering
        let before = &search_state.text[..search_state.cursor];
        let after = &search_state.text[search_state.cursor..];

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

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(Style::default().bg(THEME.dim).fg(Color::White));
    frame.render_widget(paragraph, area);
}

/// Return status bar spans showing search match info, or empty vec if no search active.
pub fn search_status_spans(search_state: &SearchState) -> Vec<Span<'static>> {
    if !search_state.active {
        return vec![];
    }
    let count = search_state.match_count();
    if count == 0 {
        vec![
            Span::raw(" "),
            Span::styled(
                format!(" /{} [no matches] ", search_state.pattern),
                Style::default().fg(THEME.level_error),
            ),
        ]
    } else {
        vec![
            Span::raw(" "),
            Span::styled(
                format!(
                    " /{} [match {}/{}] ",
                    search_state.pattern,
                    search_state.current_match_display(),
                    count,
                ),
                Style::default().fg(THEME.level_warn),
            ),
        ]
    }
}

/// Style for highlighting search matches (non-current).
pub fn match_highlight_style() -> Style {
    Style::default()
        .bg(THEME.search_match_bg)
        .fg(THEME.search_match_fg)
}

/// Style for highlighting the current search match (where n/N is focused).
pub fn current_match_highlight_style() -> Style {
    Style::default()
        .bg(THEME.search_match_bg)
        .fg(THEME.search_match_fg)
        .add_modifier(Modifier::BOLD)
}

/// Apply search highlighting to a line of text.
///
/// Takes the original text and its base style, finds match ranges for `pattern`,
/// and returns a vector of styled spans with highlights overlaid.
///
/// `is_current_match` indicates whether this entry is the current n/N focus,
/// which gets a more prominent highlight style.
pub fn highlight_line(text: &str, pattern: &str, is_current_match: bool) -> Vec<Span<'static>> {
    let ranges = find_match_ranges(text, pattern);
    if ranges.is_empty() {
        return vec![Span::raw(text.to_string())];
    }

    let hl_style = if is_current_match {
        current_match_highlight_style()
    } else {
        match_highlight_style()
    };

    let mut spans = Vec::new();
    let mut pos = 0;
    for range in &ranges {
        if range.start > pos {
            spans.push(Span::raw(text[pos..range.start].to_string()));
        }
        spans.push(Span::styled(
            text[range.start..range.end].to_string(),
            hl_style,
        ));
        pos = range.end;
    }
    if pos < text.len() {
        spans.push(Span::raw(text[pos..].to_string()));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- SearchState tests --

    #[test]
    fn new_state_is_empty() {
        let state = SearchState::new();
        assert!(state.text.is_empty());
        assert_eq!(state.cursor, 0);
        assert!(!state.active);
        assert!(state.pattern.is_empty());
        assert!(state.match_indices.is_empty());
        assert_eq!(state.current_match, 0);
    }

    #[test]
    fn insert_char_updates_text_and_cursor() {
        let mut state = SearchState::new();
        state.insert_char('h');
        state.insert_char('i');
        assert_eq!(state.text, "hi");
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn backspace_deletes_before_cursor() {
        let mut state = SearchState::new();
        state.insert_char('a');
        state.insert_char('b');
        state.insert_char('c');
        state.delete_char_before();
        assert_eq!(state.text, "ab");
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut state = SearchState::new();
        state.delete_char_before();
        assert!(state.text.is_empty());
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn move_left_right() {
        let mut state = SearchState::new();
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
    fn clear_input_resets_text() {
        let mut state = SearchState::new();
        state.insert_char('x');
        state.insert_char('y');
        state.clear_input();
        assert!(state.text.is_empty());
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn confirm_sets_pattern_and_active() {
        let mut state = SearchState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.confirm();
        assert!(state.active);
        assert_eq!(state.pattern, "error");
    }

    #[test]
    fn confirm_empty_deactivates() {
        let mut state = SearchState::new();
        state.confirm();
        assert!(!state.active);
        assert!(state.pattern.is_empty());
    }

    #[test]
    fn cancel_clears_everything() {
        let mut state = SearchState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.confirm();
        assert!(state.active);

        state.cancel();
        assert!(!state.active);
        assert!(state.text.is_empty());
        assert!(state.pattern.is_empty());
        assert!(state.match_indices.is_empty());
    }

    #[test]
    fn scan_matches_finds_entries() {
        let mut state = SearchState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.confirm();

        let texts = vec![
            (0, "INFO: started"),
            (1, "ERROR: disk full"),
            (2, "INFO: ok"),
            (3, "error: another problem"),
        ];
        state.scan_matches(texts.into_iter());
        assert_eq!(state.match_indices, vec![1, 3]);
    }

    #[test]
    fn scan_matches_case_insensitive() {
        let mut state = SearchState::new();
        for ch in "Error".chars() {
            state.insert_char(ch);
        }
        state.confirm();

        let texts = vec![(0, "ERROR: fail"), (1, "error: problem"), (2, "no match")];
        state.scan_matches(texts.into_iter());
        assert_eq!(state.match_indices, vec![0, 1]);
    }

    #[test]
    fn next_prev_match_cycles() {
        let mut state = SearchState::new();
        for ch in "x".chars() {
            state.insert_char(ch);
        }
        state.confirm();
        state.match_indices = vec![2, 5, 8];
        state.current_match = 0;

        // next cycles forward
        assert_eq!(state.next_match(), Some(5));
        assert_eq!(state.current_match, 1);
        assert_eq!(state.next_match(), Some(8));
        assert_eq!(state.current_match, 2);
        // wrap around
        assert_eq!(state.next_match(), Some(2));
        assert_eq!(state.current_match, 0);

        // prev cycles backward
        assert_eq!(state.prev_match(), Some(8));
        assert_eq!(state.current_match, 2);
        assert_eq!(state.prev_match(), Some(5));
        assert_eq!(state.current_match, 1);
    }

    #[test]
    fn next_prev_no_matches() {
        let mut state = SearchState::new();
        assert_eq!(state.next_match(), None);
        assert_eq!(state.prev_match(), None);
    }

    #[test]
    fn jump_to_nearest_finds_match() {
        let mut state = SearchState::new();
        state.match_indices = vec![3, 7, 12];
        state.current_match = 0;

        // Jump from visible index 5 — should land on 7
        assert_eq!(state.jump_to_nearest(5), Some(7));
        assert_eq!(state.current_match, 1);

        // Jump from visible index 0 — should land on 3
        assert_eq!(state.jump_to_nearest(0), Some(3));
        assert_eq!(state.current_match, 0);

        // Jump from visible index 15 — wraps to first (3)
        assert_eq!(state.jump_to_nearest(15), Some(3));
        assert_eq!(state.current_match, 0);
    }

    #[test]
    fn check_new_entry_adds_match() {
        let mut state = SearchState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.confirm();
        state.match_indices.clear();

        state.check_new_entry(0, "INFO: ok");
        assert!(state.match_indices.is_empty());

        state.check_new_entry(1, "ERROR: fail");
        assert_eq!(state.match_indices, vec![1]);
    }

    #[test]
    fn check_new_entry_inactive_noop() {
        let mut state = SearchState::new();
        state.check_new_entry(0, "ERROR: fail");
        assert!(state.match_indices.is_empty());
    }

    // -- find_match_ranges tests --

    #[test]
    fn find_ranges_basic() {
        let ranges = find_match_ranges("hello world hello", "hello");
        assert_eq!(ranges, vec![0..5, 12..17]);
    }

    #[test]
    fn find_ranges_case_insensitive() {
        let ranges = find_match_ranges("Hello HELLO hello", "hello");
        assert_eq!(ranges.len(), 3);
    }

    #[test]
    fn find_ranges_empty_pattern() {
        let ranges = find_match_ranges("hello", "");
        assert!(ranges.is_empty());
    }

    #[test]
    fn find_ranges_no_match() {
        let ranges = find_match_ranges("hello world", "xyz");
        assert!(ranges.is_empty());
    }

    // -- highlight_line tests --

    #[test]
    fn highlight_no_match() {
        let spans = highlight_line("hello world", "xyz", false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "hello world");
    }

    #[test]
    fn highlight_single_match() {
        let spans = highlight_line("hello world", "world", false);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "hello ");
        assert_eq!(spans[1].content, "world");
        assert_eq!(spans[1].style, match_highlight_style());
    }

    #[test]
    fn highlight_current_match_is_bold() {
        let spans = highlight_line("hello world", "world", true);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].style, current_match_highlight_style());
    }

    #[test]
    fn highlight_multiple_matches() {
        let spans = highlight_line("error and error again", "error", false);
        // Should be: "error", " and ", "error", " again"
        // Actually: highlighted "error", " and ", highlighted "error", " again"
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].content, "error");
        assert_eq!(spans[0].style, match_highlight_style());
        assert_eq!(spans[1].content, " and ");
        assert_eq!(spans[2].content, "error");
        assert_eq!(spans[2].style, match_highlight_style());
        assert_eq!(spans[3].content, " again");
    }

    #[test]
    fn highlight_at_start_and_end() {
        let spans = highlight_line("err", "err", false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "err");
        assert_eq!(spans[0].style, match_highlight_style());
    }

    // -- Status display tests --

    #[test]
    fn status_spans_inactive() {
        let state = SearchState::new();
        let spans = search_status_spans(&state);
        assert!(spans.is_empty());
    }

    #[test]
    fn status_spans_no_matches() {
        let mut state = SearchState::new();
        for ch in "xyz".chars() {
            state.insert_char(ch);
        }
        state.confirm();
        // No scan done, so match_indices is empty
        let spans = search_status_spans(&state);
        assert_eq!(spans.len(), 2);
        // Should contain "no matches"
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("no matches"));
    }

    #[test]
    fn status_spans_with_matches() {
        let mut state = SearchState::new();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.confirm();
        state.match_indices = vec![1, 5, 10];
        state.current_match = 1;
        let spans = search_status_spans(&state);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("2/3")); // 1-based: match 2 of 3
    }
}
