//! Shared text-input control for the filter and search panels.
//!
//! Owns the edit buffer, cursor, revert snapshot, committed history, and a
//! virtual "current edit" slot that lets the user navigate Up/Down through
//! history without losing what they were typing.
//!
//! Design: see `docs/plans/2026-05-10-filter-search-redesign.md`.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::theme::THEME;

/// Position within the history sequence `[saved..., virtual, blank]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HistoryPos {
    Saved(usize),
    Virtual,
    Blank,
}

/// A text-input control with virtual-slot history.
pub struct TextInput {
    /// Currently displayed text (what the user sees).
    pub text: String,
    /// Cursor byte offset; always on a char boundary.
    pub cursor: usize,
    /// Snapshot taken when entering input mode. Restored on revert.
    pub saved_text: String,
    /// Committed history entries, oldest first, newest last.
    pub history: Vec<String>,
    /// The value held in the virtual slot when the position is elsewhere.
    /// When `position == Virtual`, this is kept in sync with `text` after edits.
    pub virtual_text: String,
    /// Current position within the history sequence.
    pub position: HistoryPos,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            saved_text: String::new(),
            history: Vec::new(),
            virtual_text: String::new(),
            position: HistoryPos::Virtual,
        }
    }

    /// Snapshot the current text for revert and reset the in-session state.
    /// Call this when the input panel is opened.
    pub fn save_current(&mut self) {
        self.saved_text = self.text.clone();
        self.virtual_text = self.text.clone();
        self.position = HistoryPos::Virtual;
    }

    /// Restore the saved snapshot. Call this on Esc to revert.
    pub fn revert(&mut self) {
        self.text = self.saved_text.clone();
        self.cursor = self.text.len();
        self.virtual_text = self.saved_text.clone();
        self.position = HistoryPos::Virtual;
    }

    /// Commit the current text to history with MRU dedup. Empty text is not
    /// pushed. Returns the committed text (which equals `self.text`).
    pub fn commit(&mut self) -> String {
        let text = self.text.clone();
        if !text.is_empty() {
            self.history.retain(|s| s != &text);
            self.history.push(text.clone());
        }
        // After commit, reset history navigation state for the next session.
        self.virtual_text = self.text.clone();
        self.position = HistoryPos::Virtual;
        text
    }

    /// Any mutation snaps the position back to virtual: edits target only
    /// the virtual slot, never saved entries.
    fn snap_to_virtual(&mut self) {
        self.position = HistoryPos::Virtual;
    }

    pub fn insert_char(&mut self, ch: char) {
        self.snap_to_virtual();
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.virtual_text = self.text.clone();
    }

    pub fn delete_char_before(&mut self) {
        self.snap_to_virtual();
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.remove(prev);
            self.cursor = prev;
            self.virtual_text = self.text.clone();
        }
    }

    pub fn clear(&mut self) {
        self.snap_to_virtual();
        self.text.clear();
        self.cursor = 0;
        self.virtual_text.clear();
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Replace the input text wholesale and place cursor at end.
    /// Treats this as an edit (snaps to virtual).
    pub fn set_text(&mut self, text: String) {
        self.snap_to_virtual();
        self.text = text;
        self.cursor = self.text.len();
        self.virtual_text = self.text.clone();
    }

    /// Move the position one step toward older history.
    /// From Blank → Virtual; from Virtual → newest saved (if any);
    /// from Saved(i) → Saved(i-1) (clamps at oldest).
    ///
    /// If the virtual slot equals the newest saved entry (which happens
    /// whenever the panel reopens with the previously-committed filter
    /// already loaded), the newest saved entry is skipped — Up lands on
    /// the entry *before* the duplicate so the user doesn't have to press
    /// Up twice to get visible motion.
    pub fn history_up(&mut self) {
        match self.position {
            HistoryPos::Blank => {
                self.position = HistoryPos::Virtual;
                self.text = self.virtual_text.clone();
                self.cursor = self.text.len();
            }
            HistoryPos::Virtual => {
                if let Some(last) = self.history.last() {
                    if last == &self.virtual_text {
                        // Newest saved == virtual. Skip past the duplicate.
                        // If there's no entry before it, stay put.
                        if self.history.len() >= 2 {
                            let idx = self.history.len() - 2;
                            self.position = HistoryPos::Saved(idx);
                            self.text = self.history[idx].clone();
                            self.cursor = self.text.len();
                        }
                    } else {
                        let idx = self.history.len() - 1;
                        self.position = HistoryPos::Saved(idx);
                        self.text = self.history[idx].clone();
                        self.cursor = self.text.len();
                    }
                }
            }
            HistoryPos::Saved(idx) => {
                if idx > 0 {
                    let new_idx = idx - 1;
                    self.position = HistoryPos::Saved(new_idx);
                    self.text = self.history[new_idx].clone();
                    self.cursor = self.text.len();
                }
            }
        }
    }

    /// Move the position one step toward blank.
    /// From Saved(i) → Saved(i+1) or → Virtual past newest;
    /// from Virtual → Blank; from Blank → Blank (clamps).
    ///
    /// Symmetric with `history_up`: when stepping into Saved(last) would
    /// land on a value equal to the virtual slot, the duplicate is skipped
    /// and the position goes straight to Virtual.
    pub fn history_down(&mut self) {
        match self.position {
            HistoryPos::Saved(idx) => {
                let next_idx = idx + 1;
                if next_idx < self.history.len() {
                    let lands_on_duplicate = next_idx == self.history.len() - 1
                        && self.history[next_idx] == self.virtual_text;
                    if lands_on_duplicate {
                        self.position = HistoryPos::Virtual;
                        self.text = self.virtual_text.clone();
                        self.cursor = self.text.len();
                    } else {
                        self.position = HistoryPos::Saved(next_idx);
                        self.text = self.history[next_idx].clone();
                        self.cursor = self.text.len();
                    }
                } else {
                    self.position = HistoryPos::Virtual;
                    self.text = self.virtual_text.clone();
                    self.cursor = self.text.len();
                }
            }
            HistoryPos::Virtual => {
                self.position = HistoryPos::Blank;
                self.text.clear();
                self.cursor = 0;
            }
            HistoryPos::Blank => {}
        }
    }
}

/// Options for rendering the bordered chrome around a text input.
pub struct ChromeOptions<'a> {
    /// Title shown in the top border (e.g., "filter", "search").
    pub title: &'a str,
    /// Hint string shown in the bottom border (e.g., "[enter] save  [esc] cancel").
    pub hints: &'a str,
    /// Optional trailing content shown after the input text (e.g., parse error).
    pub trailing: Option<Span<'a>>,
    /// Placeholder shown when the input is empty.
    pub placeholder: &'a str,
}

/// Render a `TextInput` inside a rounded bordered box with title + hint chrome.
///
/// The control is always focused while visible; there is no idle state.
/// Takes 3 rows (border-top + content + border-bottom).
pub fn render_chrome(frame: &mut Frame, area: Rect, input: &TextInput, opts: ChromeOptions) {
    // Build the input line (text + cursor + optional trailing content).
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));

    if input.text.is_empty() {
        spans.push(Span::styled(
            opts.placeholder.to_string(),
            Style::default().fg(THEME.dim),
        ));
        // Show the cursor as a block at the start.
        spans.insert(
            1,
            Span::styled(" ", Style::default().fg(Color::Black).bg(Color::White)),
        );
    } else {
        let before = &input.text[..input.cursor];
        let after = &input.text[input.cursor..];

        spans.push(Span::styled(
            before.to_string(),
            Style::default().fg(Color::White),
        ));

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

    if let Some(trailing) = opts.trailing {
        spans.push(Span::raw("  "));
        spans.push(trailing);
    }

    // Top title and bottom hints embedded in the border edges.
    let title = Span::styled(
        format!(" {} ", opts.title),
        Style::default().fg(THEME.accent),
    );
    let hints = Span::styled(
        format!(" {} ", opts.hints),
        Style::default().fg(THEME.dim),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(THEME.accent))
        .title_top(title)
        .title_bottom(hints);

    let paragraph = Paragraph::new(Line::from(spans)).block(block);
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk through the worked examples from the design doc.
    #[test]
    fn doc_example_1_foo_down_up() {
        // f 'foo' down up → 'foo'
        let mut t = TextInput::new();
        t.save_current(); // entering panel with empty active filter
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        assert_eq!(t.text, "foo");
        t.history_down();
        assert_eq!(t.text, ""); // at blank
        t.history_up();
        assert_eq!(t.text, "foo"); // back to virtual
    }

    #[test]
    fn doc_example_2_foo_down_bar_up() {
        // f 'foo' down 'bar' up → 'bar'
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.history_down();
        assert_eq!(t.text, "");
        for ch in "bar".chars() {
            t.insert_char(ch);
        }
        // Typing at blank snapped to virtual and overwrote it
        assert_eq!(t.text, "bar");
        t.history_up();
        // No saved history yet — stays at virtual='bar'
        assert_eq!(t.text, "bar");
    }

    #[test]
    fn doc_example_3_foo_enter_d() {
        // f 'foo' enter f 'd' → 'food'
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit(); // saves 'foo' to history
        // Reopen — text is still 'foo' (the active filter)
        t.save_current();
        // Cursor would be moved to end by the wrapper; simulate
        t.cursor = t.text.len();
        t.insert_char('d');
        assert_eq!(t.text, "food");
    }

    #[test]
    fn doc_example_4_foo_enter_d_up() {
        // f 'foo' enter f 'd' up → 'foo' (and virtual preserved)
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        t.cursor = t.text.len();
        t.insert_char('d');
        assert_eq!(t.text, "food");
        t.history_up();
        // Stepped into saved[0]='foo'
        assert_eq!(t.text, "foo");
        // Virtual preserved — Down returns to 'food'
        t.history_down();
        assert_eq!(t.text, "food");
    }

    #[test]
    fn mru_dedup_on_commit() {
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        t.clear();
        for ch in "bar".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        t.clear();
        // Re-apply 'foo' — should move it to newest, not duplicate
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        assert_eq!(t.history, vec!["bar".to_string(), "foo".to_string()]);
    }

    #[test]
    fn empty_commit_does_not_push() {
        let mut t = TextInput::new();
        t.save_current();
        t.commit();
        assert!(t.history.is_empty());
    }

    #[test]
    fn edited_entry_is_distinct() {
        // saved=['foo','bar'], Up to 'foo', edit to 'foobaz', Enter
        // → saved=['bar','foo','foobaz']
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        t.clear();
        for ch in "bar".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        t.clear();
        // Up twice: virtual → saved[1]='bar' → saved[0]='foo'
        t.history_up();
        assert_eq!(t.text, "bar");
        t.history_up();
        assert_eq!(t.text, "foo");
        // Typing while on saved[0] snaps position to virtual but the displayed
        // text stays 'foo'; chars insert at cursor (which is at end after nav).
        for ch in "baz".chars() {
            t.insert_char(ch);
        }
        assert_eq!(t.text, "foobaz");
        assert_eq!(t.virtual_text, "foobaz");
        t.commit();
        assert_eq!(
            t.history,
            vec!["foo".to_string(), "bar".to_string(), "foobaz".to_string()]
        );
    }

    #[test]
    fn clamp_at_oldest_and_blank() {
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        // After commit, virtual_text='foo' and history=['foo'].
        // Clear the virtual slot so it differs from the saved entry,
        // otherwise Up will skip the duplicate.
        t.clear();
        // Down past virtual goes to blank, then stays at blank
        t.history_down(); // virtual → blank
        t.history_down(); // stays
        assert_eq!(t.text, "");
        assert_eq!(t.position, HistoryPos::Blank);
        // Up walks back through virtual → saved[0]
        t.history_up(); // → virtual (empty)
        assert_eq!(t.text, "");
        t.history_up(); // → saved[0]='foo'
        assert_eq!(t.position, HistoryPos::Saved(0));
        t.history_up(); // clamps at oldest
        assert_eq!(t.position, HistoryPos::Saved(0));
    }

    #[test]
    fn up_skips_duplicate_of_virtual_at_newest_saved() {
        // Reproducer for the f→'heart'→Enter→f→Up bug: when the panel
        // reopens with the active text equal to history.last(), the first
        // Up should land on the entry before that duplicate.
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        t.clear();
        for ch in "heart".chars() {
            t.insert_char(ch);
        }
        t.commit();
        // Reopen the panel — text='heart', virtual_text='heart',
        // history=['foo','heart'].
        t.save_current();
        assert_eq!(t.text, "heart");
        assert_eq!(t.virtual_text, "heart");

        // One Up should skip 'heart' and land on 'foo'.
        t.history_up();
        assert_eq!(t.text, "foo");
        assert_eq!(t.position, HistoryPos::Saved(0));

        // Down should mirror Up: skip the duplicate and land back on virtual.
        t.history_down();
        assert_eq!(t.text, "heart");
        assert_eq!(t.position, HistoryPos::Virtual);

        // Another Down goes to Blank as usual.
        t.history_down();
        assert!(t.text.is_empty());
        assert_eq!(t.position, HistoryPos::Blank);
    }

    #[test]
    fn up_stays_when_only_entry_equals_virtual() {
        // Single-entry history that equals virtual: nowhere to go, stay put.
        let mut t = TextInput::new();
        t.save_current();
        for ch in "foo".chars() {
            t.insert_char(ch);
        }
        t.commit();
        t.save_current();
        // text='foo', virtual='foo', history=['foo']
        t.history_up();
        assert_eq!(t.text, "foo");
        assert_eq!(t.position, HistoryPos::Virtual);
    }
}
