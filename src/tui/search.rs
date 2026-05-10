//! Search state for the TUI (`/` find with n/N navigation).
//!
//! Wraps the shared `TextInput` control and tracks match positions in the
//! currently-visible log. The text input portion (chrome, history, virtual
//! slot) is shared with the filter panel; this module owns the search-
//! specific match tracking, highlighting, and navigation.

use std::ops::Range;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};

use crate::theme::THEME;
use crate::tui::text_input::{ChromeOptions, TextInput, render_chrome};

/// Hint string shown in the bottom border of the search input box.
const SEARCH_HINTS: &str = "[enter] save  [esc] cancel";

/// Placeholder shown in the input when empty.
const SEARCH_PLACEHOLDER: &str = "search...";

/// State for the search feature.
pub struct SearchState {
    /// Shared text-input control: text, cursor, history, virtual slot.
    pub input: TextInput,
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
            input: TextInput::new(),
            active: false,
            pattern: String::new(),
            match_indices: Vec::new(),
            current_match: 0,
        }
    }

    /// Save the current state before entering search input mode.
    /// Pre-populates the input with the active pattern so editing extends it.
    pub fn save_current(&mut self) {
        self.input.text = self.pattern.clone();
        self.input.cursor = self.input.text.len();
        self.input.save_current();
    }

    /// Revert to the saved snapshot. Active search pattern is unchanged
    /// (the snapshot equals the pre-edit pattern).
    pub fn revert(&mut self) {
        self.input.revert();
    }

    /// Commit the current input text as the active pattern and push to
    /// history with MRU dedup. Sets `active` and clears match state for
    /// the caller to re-populate via `scan_matches`.
    pub fn commit(&mut self) {
        self.input.commit();
        self.pattern = self.input.text.clone();
        if self.pattern.is_empty() {
            self.active = false;
            self.match_indices.clear();
            self.current_match = 0;
        } else {
            self.active = true;
        }
    }

    /// Clear the active search entirely (used by Esc-from-normal-mode).
    pub fn clear_active(&mut self) {
        self.input.text.clear();
        self.input.cursor = 0;
        self.input.saved_text.clear();
        self.input.virtual_text.clear();
        self.active = false;
        self.pattern.clear();
        self.match_indices.clear();
        self.current_match = 0;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.input.insert_char(ch);
    }

    pub fn delete_char_before(&mut self) {
        self.input.delete_char_before();
    }

    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    pub fn history_up(&mut self) {
        self.input.history_up();
    }

    pub fn history_down(&mut self) {
        self.input.history_down();
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

    /// Find the nearest match at or after the given visible index.
    pub fn jump_to_nearest(&mut self, visible_index: usize) -> Option<usize> {
        if self.match_indices.is_empty() {
            return None;
        }
        for (i, &idx) in self.match_indices.iter().enumerate() {
            if idx >= visible_index {
                self.current_match = i;
                return Some(idx);
            }
        }
        self.current_match = 0;
        Some(self.match_indices[0])
    }

    pub fn match_count(&self) -> usize {
        self.match_indices.len()
    }

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

/// Render the search input bar (bordered chrome with title + hints).
pub fn render_search_input(frame: &mut Frame, area: Rect, search_state: &SearchState) {
    render_chrome(
        frame,
        area,
        &search_state.input,
        ChromeOptions {
            title: "search",
            hints: SEARCH_HINTS,
            trailing: None,
            placeholder: SEARCH_PLACEHOLDER,
        },
    );
}

/// Return status bar spans showing search match info as a chip,
/// or empty vec if no search active.
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
                Style::default()
                    .fg(Color::Black)
                    .bg(THEME.level_error),
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
                Style::default().fg(Color::Black).bg(THEME.level_warn),
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

    #[test]
    fn new_state_is_empty() {
        let state = SearchState::new();
        assert!(state.input.text.is_empty());
        assert_eq!(state.input.cursor, 0);
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
        assert_eq!(state.input.text, "hi");
        assert_eq!(state.input.cursor, 2);
    }

    #[test]
    fn commit_sets_pattern_and_active() {
        let mut state = SearchState::new();
        state.save_current();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.commit();
        assert_eq!(state.pattern, "error");
        assert!(state.active);
        assert_eq!(state.input.history, vec!["error".to_string()]);
    }

    #[test]
    fn commit_empty_deactivates() {
        let mut state = SearchState::new();
        state.save_current();
        state.commit();
        assert!(state.pattern.is_empty());
        assert!(!state.active);
        assert!(state.input.history.is_empty());
    }

    #[test]
    fn clear_active_resets_search() {
        let mut state = SearchState::new();
        state.save_current();
        for ch in "error".chars() {
            state.insert_char(ch);
        }
        state.commit();
        state.clear_active();
        assert!(state.input.text.is_empty());
        assert!(!state.active);
        assert!(state.pattern.is_empty());
    }

    #[test]
    fn next_prev_wrap() {
        let mut state = SearchState::new();
        state.match_indices = vec![1, 5, 9];
        state.active = true;
        state.current_match = 0;

        assert_eq!(state.next_match(), Some(5));
        assert_eq!(state.next_match(), Some(9));
        assert_eq!(state.next_match(), Some(1));

        assert_eq!(state.prev_match(), Some(9));
        assert_eq!(state.prev_match(), Some(5));
    }

    #[test]
    fn find_match_ranges_basic() {
        let ranges = find_match_ranges("the quick brown fox", "quick");
        assert_eq!(ranges, vec![4..9]);

        let ranges = find_match_ranges("ababab", "ab");
        assert_eq!(ranges, vec![0..2, 2..4, 4..6]);

        let ranges = find_match_ranges("Hello World", "world");
        assert_eq!(ranges, vec![6..11]);
    }

    #[test]
    fn find_match_ranges_empty_pattern() {
        let ranges = find_match_ranges("text", "");
        assert!(ranges.is_empty());
    }
}
