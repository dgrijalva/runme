//! Anchor-based viewport for the TUI log viewer.
//!
//! Uses a vim-style scrolling model: the cursor moves freely through entries,
//! and the viewport scrolls to keep the cursor within a margin of the edges.

use crate::log::LogEntry;

use super::render::{DisplayMode, SourceColors, render_entry};

/// Scroll margin: the cursor never gets closer than this many rows
/// to the top or bottom of the viewport.
const SCROLL_MARGIN: u16 = 2;

/// Scroll state: either following the tail or pinned at a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollState {
    /// Following the end of the log. Cursor is always the last entry.
    Tail,
    /// Pinned: cursor and viewport are independent of new entries.
    Pinned {
        /// Which entry is focused (index into visible entries)
        cursor: usize,
        /// Which entry is at the top of the viewport
        top: usize,
    },
}

/// Compute the viewport layout: which entries are visible and where.
///
/// Returns a list of (entry_index, y_position, rendered_lines, is_cursor) tuples.
/// The viewport starts at `top` entry and fills downward.
pub struct ViewportLayout {
    pub entries: Vec<ViewportEntry>,
}

#[derive(Debug, Clone)]
pub struct ViewportEntry {
    /// Index into the entries slice
    pub entry_index: usize,
    /// Y position of this entry's first line in the viewport
    pub y: u16,
    /// Rendered lines for this entry
    pub lines: Vec<ratatui::text::Line<'static>>,
    /// Total visual height of this entry
    pub height: usize,
    /// Whether this entry is the cursor (focused)
    pub is_cursor: bool,
}

/// Compute the visible entries for the viewport.
pub fn layout(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> ViewportLayout {
    if entries.is_empty() || viewport_height == 0 {
        return ViewportLayout {
            entries: Vec::new(),
        };
    }

    let (cursor, top) = match *scroll {
        ScrollState::Tail => {
            let cursor = entries.len() - 1;
            // Compute top so the last entry lands `SCROLL_MARGIN` rows above the bottom,
            // mirroring the cursor margin used when scrolling.
            let effective_height = viewport_height.saturating_sub(SCROLL_MARGIN).max(1);
            let top = compute_top_for_bottom(
                cursor,
                entries,
                effective_height,
                width,
                mode,
                wrap,
                source_colors,
            );
            (cursor, top)
        }
        ScrollState::Pinned { cursor, top } => {
            let cursor = cursor.min(entries.len() - 1);
            let top = top.min(entries.len() - 1);
            (cursor, top)
        }
    };

    // Render entries from top downward until viewport is full
    let mut result = Vec::new();
    let mut y: u16 = 0;
    let mut idx = top;

    while y < viewport_height && idx < entries.len() {
        let (lines, height) = render_entry(&entries[idx], width, mode, wrap, source_colors);
        result.push(ViewportEntry {
            entry_index: idx,
            y,
            lines,
            height,
            is_cursor: idx == cursor,
        });
        y += height as u16;
        idx += 1;
    }

    ViewportLayout { entries: result }
}

/// Given that we want `bottom_entry` to be the last visible entry,
/// compute the `top` index.
fn compute_top_for_bottom(
    bottom_entry: usize,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> usize {
    let mut consumed: u16 = 0;
    let mut idx = bottom_entry;

    loop {
        let (_, height) = render_entry(&entries[idx], width, mode, wrap, source_colors);
        consumed += height as u16;
        if consumed >= viewport_height || idx == 0 {
            break;
        }
        idx -= 1;
    }

    idx
}

/// Move cursor down one entry. Adjusts top if cursor would exceed margin.
pub fn scroll_down(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    match *scroll {
        ScrollState::Tail => ScrollState::Tail,
        ScrollState::Pinned { cursor, top } => {
            if cursor + 1 >= entries.len() {
                return ScrollState::Tail;
            }
            let new_cursor = cursor + 1;
            let new_top = adjust_top_for_cursor(
                new_cursor,
                top,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
            );
            ScrollState::Pinned {
                cursor: new_cursor,
                top: new_top,
            }
        }
    }
}

/// Move cursor up one entry. Adjusts top if cursor would exceed margin.
pub fn scroll_up(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    match *scroll {
        ScrollState::Tail => {
            if entries.len() <= 1 {
                return ScrollState::Pinned { cursor: 0, top: 0 };
            }
            let cursor = entries.len() - 2;
            let top = compute_top_for_bottom(
                entries.len() - 1,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
            );
            let top = adjust_top_for_cursor(
                cursor,
                top,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
            );
            ScrollState::Pinned { cursor, top }
        }
        ScrollState::Pinned { cursor, top } => {
            if cursor == 0 {
                return ScrollState::Pinned { cursor: 0, top: 0 };
            }
            let new_cursor = cursor - 1;
            let new_top = adjust_top_for_cursor(
                new_cursor,
                top,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
            );
            ScrollState::Pinned {
                cursor: new_cursor,
                top: new_top,
            }
        }
    }
}

/// Jump forward by half a viewport.
pub fn scroll_down_half_page(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    let cursor = match *scroll {
        ScrollState::Tail => return ScrollState::Tail,
        ScrollState::Pinned { cursor, .. } => cursor,
    };

    let half = (viewport_height / 2).max(1) as usize;
    let new_cursor = (cursor + half).min(entries.len() - 1);

    if new_cursor >= entries.len() - 1 {
        return ScrollState::Tail;
    }

    let top = match *scroll {
        ScrollState::Pinned { top, .. } => top,
        _ => 0,
    };
    let new_top = adjust_top_for_cursor(
        new_cursor,
        top,
        entries,
        viewport_height,
        width,
        mode,
        wrap,
        source_colors,
    );

    ScrollState::Pinned {
        cursor: new_cursor,
        top: new_top,
    }
}

/// Jump backward by half a viewport.
pub fn scroll_up_half_page(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    let cursor = match *scroll {
        ScrollState::Tail => entries.len() - 1,
        ScrollState::Pinned { cursor, .. } => cursor,
    };

    let half = (viewport_height / 2).max(1) as usize;
    let new_cursor = cursor.saturating_sub(half);

    let top = match *scroll {
        ScrollState::Pinned { top, .. } => top,
        ScrollState::Tail => compute_top_for_bottom(
            entries.len() - 1,
            entries,
            viewport_height,
            width,
            mode,
            wrap,
            source_colors,
        ),
    };
    let new_top = adjust_top_for_cursor(
        new_cursor,
        top,
        entries,
        viewport_height,
        width,
        mode,
        wrap,
        source_colors,
    );

    ScrollState::Pinned {
        cursor: new_cursor,
        top: new_top,
    }
}

/// Jump to first entry.
pub fn scroll_to_top(_scroll: &ScrollState, entries: &[LogEntry]) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }
    ScrollState::Pinned { cursor: 0, top: 0 }
}

/// Jump to last entry (tail mode).
pub fn scroll_to_bottom(_scroll: &ScrollState, entries: &[LogEntry]) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }
    ScrollState::Tail
}

/// Compute the number of new entries since pinning.
pub fn new_entries_since_pin(scroll: &ScrollState, total: usize) -> usize {
    match *scroll {
        ScrollState::Tail => 0,
        ScrollState::Pinned { cursor, .. } => total.saturating_sub(cursor + 1),
    }
}

/// Ensure the cursor is visible within the viewport with proper margins.
/// Returns the adjusted `top` value.
#[allow(clippy::too_many_arguments)]
fn adjust_top_for_cursor(
    cursor: usize,
    current_top: usize,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> usize {
    let margin = SCROLL_MARGIN.min(viewport_height / 2) as usize;
    let vh = viewport_height as usize;

    // In truncated mode (height=1 per entry), this is simple index math.
    // In wrapped mode, we'd need to sum heights. For now, use entry-count
    // approximation which works perfectly in truncated mode and is close enough
    // in wrapped mode for the margin check.
    if !wrap {
        // Truncated: 1 line per entry, simple index math
        let mut top = current_top;

        // Cursor too close to top? Scroll up.
        if cursor < top + margin {
            top = cursor.saturating_sub(margin);
        }

        // Cursor too close to bottom? Scroll down.
        if cursor + margin >= top + vh {
            top = (cursor + margin + 1).saturating_sub(vh);
        }

        // Don't scroll past the end
        top = top.min(entries.len().saturating_sub(1));

        top
    } else {
        // Wrapped mode: sum heights to find cursor position relative to top
        let mut y: usize = 0;
        let mut top = current_top;

        // Compute Y of cursor relative to current top
        for (i, entry) in entries
            .iter()
            .enumerate()
            .take(cursor.min(entries.len() - 1) + 1)
            .skip(top)
        {
            if i == cursor {
                break;
            }
            let (_, h) = render_entry(entry, width, mode, wrap, source_colors);
            y += h;
        }

        // Cursor above the margin
        if y < margin {
            top = cursor.saturating_sub(margin);
        }

        // Cursor below the bottom margin
        let (_, cursor_h) = render_entry(
            &entries[cursor.min(entries.len() - 1)],
            width,
            mode,
            wrap,
            source_colors,
        );
        if y + cursor_h + margin > vh {
            // Need to scroll down: recompute top
            // Walk backward from cursor to find new top
            let mut space = margin + cursor_h;
            let mut new_top = cursor;
            while new_top > 0 && space < vh {
                new_top -= 1;
                let (_, h) = render_entry(&entries[new_top], width, mode, wrap, source_colors);
                space += h;
            }
            top = new_top;
        }

        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{LogEntry, ParsedContent};
    use std::collections::HashMap;

    fn make_entry(raw: &str, source: &str) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq: 0,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(raw.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    fn make_entries(n: usize) -> Vec<LogEntry> {
        (0..n)
            .map(|i| make_entry(&format!("entry {}", i), "test"))
            .collect()
    }

    #[test]
    fn layout_empty() {
        let mut sc = SourceColors::new();
        let result = layout(
            &ScrollState::Tail,
            &[],
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        assert!(result.entries.is_empty());
    }

    #[test]
    fn layout_tail_single() {
        let entries = make_entries(1);
        let mut sc = SourceColors::new();
        let result = layout(
            &ScrollState::Tail,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        assert_eq!(result.entries.len(), 1);
        assert!(result.entries[0].is_cursor);
    }

    #[test]
    fn layout_tail_fills_viewport() {
        let entries = make_entries(30);
        let mut sc = SourceColors::new();
        let result = layout(
            &ScrollState::Tail,
            &entries,
            10,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        // Should show ~10 entries (last ones)
        assert!(result.entries.len() <= 10);
        // Last entry should be the cursor
        let last = result.entries.last().unwrap();
        assert!(last.is_cursor);
        assert_eq!(last.entry_index, 29);
    }

    #[test]
    fn scroll_down_moves_cursor() {
        let entries = make_entries(20);
        let mut sc = SourceColors::new();
        let state = ScrollState::Pinned { cursor: 5, top: 0 };
        let next = scroll_down(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        match next {
            ScrollState::Pinned { cursor, .. } => assert_eq!(cursor, 6),
            _ => panic!("expected Pinned"),
        }
    }

    #[test]
    fn scroll_down_at_end_goes_tail() {
        let entries = make_entries(10);
        let mut sc = SourceColors::new();
        let state = ScrollState::Pinned { cursor: 9, top: 0 };
        let next = scroll_down(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        assert_eq!(next, ScrollState::Tail);
    }

    #[test]
    fn scroll_up_moves_cursor() {
        let entries = make_entries(20);
        let mut sc = SourceColors::new();
        let state = ScrollState::Pinned { cursor: 10, top: 5 };
        let next = scroll_up(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        match next {
            ScrollState::Pinned { cursor, .. } => assert_eq!(cursor, 9),
            _ => panic!("expected Pinned"),
        }
    }

    #[test]
    fn scroll_up_from_tail() {
        let entries = make_entries(20);
        let mut sc = SourceColors::new();
        let next = scroll_up(
            &ScrollState::Tail,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        match next {
            ScrollState::Pinned { cursor, .. } => assert_eq!(cursor, 18),
            _ => panic!("expected Pinned"),
        }
    }

    #[test]
    fn scroll_up_at_top_stays() {
        let entries = make_entries(10);
        let mut sc = SourceColors::new();
        let state = ScrollState::Pinned { cursor: 0, top: 0 };
        let next = scroll_up(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        assert_eq!(next, ScrollState::Pinned { cursor: 0, top: 0 });
    }

    #[test]
    fn scroll_to_top_works() {
        let entries = make_entries(20);
        let next = scroll_to_top(
            &ScrollState::Pinned {
                cursor: 15,
                top: 10,
            },
            &entries,
        );
        assert_eq!(next, ScrollState::Pinned { cursor: 0, top: 0 });
    }

    #[test]
    fn scroll_to_bottom_works() {
        let entries = make_entries(20);
        let next = scroll_to_bottom(&ScrollState::Pinned { cursor: 5, top: 0 }, &entries);
        assert_eq!(next, ScrollState::Tail);
    }

    #[test]
    fn cursor_highlight_in_layout() {
        let entries = make_entries(10);
        let mut sc = SourceColors::new();
        let state = ScrollState::Pinned { cursor: 3, top: 0 };
        let result = layout(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        // Entry 3 should be the cursor
        let cursor_entry = result.entries.iter().find(|e| e.entry_index == 3).unwrap();
        assert!(cursor_entry.is_cursor);
        // Others should not be cursor
        for e in &result.entries {
            if e.entry_index != 3 {
                assert!(!e.is_cursor);
            }
        }
    }

    #[test]
    fn margin_adjusts_top() {
        let entries = make_entries(30);
        let mut sc = SourceColors::new();
        // Cursor at 15, viewport height 10, top at 0 — cursor is way below viewport
        let top = adjust_top_for_cursor(
            15,
            0,
            &entries,
            10,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
        );
        // top should adjust so cursor is visible with margin
        assert!(top > 0);
        assert!(15 >= top + SCROLL_MARGIN as usize);
        assert!(15 + SCROLL_MARGIN as usize <= top + 10);
    }

    #[test]
    fn new_entries_since_pin_tail() {
        assert_eq!(new_entries_since_pin(&ScrollState::Tail, 100), 0);
    }

    #[test]
    fn new_entries_since_pin_pinned() {
        let state = ScrollState::Pinned {
            cursor: 50,
            top: 40,
        };
        assert_eq!(new_entries_since_pin(&state, 100), 49);
    }
}
