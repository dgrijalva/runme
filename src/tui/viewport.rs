//! Anchor-based viewport for the TUI log viewer.
//!
//! Uses a vim-style scrolling model: the cursor moves freely through entries,
//! and the viewport scrolls to keep the cursor within a margin of the edges.

use std::collections::HashMap;

use crate::execution::TaskId;
use crate::log::LogEntry;
use crate::log::field_stats::FieldStats;

use super::render::{DisplayMode, SourceColors, render_entry, render_entry_opts};

/// Scroll margin: the cursor never gets closer than this many rows
/// to the top or bottom of the viewport.
const SCROLL_MARGIN: u16 = 2;

/// Scroll state: either following the tail or pinned at a position.
///
/// `Pinned` stores entry seq numbers, not visible-list indices. This makes
/// the cursor stable across visible-set changes (filter applied, source
/// toggled, focus changed): if the pinned entry survives, the cursor stays
/// on it; if not, [`resolve_seq`] picks the nearest surviving entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollState {
    /// Following the end of the log. Cursor is always the last entry.
    Tail,
    /// Pinned by entry identity. `cursor_seq` is the seq of the focused entry;
    /// `top_seq` is the seq of the entry at the top of the viewport.
    Pinned {
        cursor_seq: u64,
        top_seq: u64,
    },
}

/// Resolved pinned position as visible-list indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub cursor: usize,
    pub top: usize,
}

impl ScrollState {
    /// Resolve to visible-list indices against the current `entries` slice
    /// (must be sorted by `seq` ascending). Returns `None` if `entries` is
    /// empty. For `Tail`, both `cursor` and `top` point at the last entry;
    /// `layout` recomputes `top` from `cursor` for the Tail case anyway.
    pub fn resolve(&self, entries: &[LogEntry]) -> Option<Resolved> {
        if entries.is_empty() {
            return None;
        }
        match *self {
            ScrollState::Tail => {
                let last = entries.len() - 1;
                Some(Resolved {
                    cursor: last,
                    top: last,
                })
            }
            ScrollState::Pinned {
                cursor_seq,
                top_seq,
            } => Some(Resolved {
                cursor: resolve_seq(entries, cursor_seq),
                top: resolve_seq(entries, top_seq),
            }),
        }
    }

    /// Convenience: resolve and return the cursor index, or `None` if empty.
    pub fn cursor_index(&self, entries: &[LogEntry]) -> Option<usize> {
        self.resolve(entries).map(|r| r.cursor)
    }

    /// Construct a `Pinned` state from visible-list indices, snapshotting the
    /// seqs of the entries at those positions. Returns `Tail` if `entries` is
    /// empty.
    pub fn pinned(entries: &[LogEntry], cursor: usize, top: usize) -> Self {
        if entries.is_empty() {
            return ScrollState::Tail;
        }
        let last = entries.len() - 1;
        let cursor = cursor.min(last);
        let top = top.min(last);
        ScrollState::Pinned {
            cursor_seq: entries[cursor].seq,
            top_seq: entries[top].seq,
        }
    }
}

/// Resolve a `seq` to an index in `entries`. Returns the index of the entry
/// with matching seq, or — if not present — the nearest surviving entry
/// (preferring the next-larger seq, falling back to the last entry).
///
/// Caller must ensure `entries` is non-empty and sorted by `seq` ascending.
pub fn resolve_seq(entries: &[LogEntry], seq: u64) -> usize {
    debug_assert!(!entries.is_empty());
    let pos = entries.partition_point(|e| e.seq < seq);
    pos.min(entries.len() - 1)
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
#[allow(clippy::too_many_arguments)]
pub fn layout(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
    field_stats: Option<&FieldStats>,
    show_fields: bool,
    source_labels: &HashMap<TaskId, String>,
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
                source_labels,
            );
            (cursor, top)
        }
        ScrollState::Pinned { .. } => {
            let r = scroll.resolve(entries).expect("entries non-empty");
            // After visible-set changes, the resolved `top` may no longer
            // keep the cursor in view (e.g. `top` resolved to the same entry
            // as `cursor` because the originals were filtered out). Always
            // adjust to keep the cursor visible with margin.
            let top = adjust_top_for_cursor(
                r.cursor,
                r.top,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
                source_labels,
            );
            (r.cursor, top)
        }
    };

    // Render entries from top downward until viewport is full
    let mut result = Vec::new();
    let mut y: u16 = 0;
    let mut idx = top;

    while y < viewport_height && idx < entries.len() {
        let (lines, height) = render_entry_opts(
            &entries[idx],
            width,
            mode,
            wrap,
            source_colors,
            field_stats,
            show_fields,
            source_labels,
        );
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
#[allow(clippy::too_many_arguments)]
fn compute_top_for_bottom(
    bottom_entry: usize,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
    source_labels: &HashMap<TaskId, String>,
) -> usize {
    let mut consumed: u16 = 0;
    let mut idx = bottom_entry;

    loop {
        let (_, height) = render_entry(
            &entries[idx],
            width,
            mode,
            wrap,
            source_colors,
            None,
            source_labels,
        );
        consumed += height as u16;
        if consumed >= viewport_height || idx == 0 {
            break;
        }
        idx -= 1;
    }

    idx
}

/// Move cursor down one entry. Adjusts top if cursor would exceed margin.
#[allow(clippy::too_many_arguments)]
pub fn scroll_down(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
    source_labels: &HashMap<TaskId, String>,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    match *scroll {
        ScrollState::Tail => ScrollState::Tail,
        ScrollState::Pinned { .. } => {
            let r = scroll.resolve(entries).expect("entries non-empty");
            if r.cursor + 1 >= entries.len() {
                return ScrollState::Tail;
            }
            let new_cursor = r.cursor + 1;
            let new_top = adjust_top_for_cursor(
                new_cursor,
                r.top,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
                source_labels,
            );
            ScrollState::pinned(entries, new_cursor, new_top)
        }
    }
}

/// Move cursor up one entry. Adjusts top if cursor would exceed margin.
#[allow(clippy::too_many_arguments)]
pub fn scroll_up(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
    source_labels: &HashMap<TaskId, String>,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    match *scroll {
        ScrollState::Tail => {
            if entries.len() <= 1 {
                return ScrollState::pinned(entries, 0, 0);
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
                source_labels,
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
                source_labels,
            );
            ScrollState::pinned(entries, cursor, top)
        }
        ScrollState::Pinned { .. } => {
            let r = scroll.resolve(entries).expect("entries non-empty");
            if r.cursor == 0 {
                return ScrollState::pinned(entries, 0, 0);
            }
            let new_cursor = r.cursor - 1;
            let new_top = adjust_top_for_cursor(
                new_cursor,
                r.top,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
                source_labels,
            );
            ScrollState::pinned(entries, new_cursor, new_top)
        }
    }
}

/// Jump forward by half a viewport.
#[allow(clippy::too_many_arguments)]
pub fn scroll_down_half_page(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
    source_labels: &HashMap<TaskId, String>,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    let r = match *scroll {
        ScrollState::Tail => return ScrollState::Tail,
        ScrollState::Pinned { .. } => scroll.resolve(entries).expect("entries non-empty"),
    };

    let half = (viewport_height / 2).max(1) as usize;
    let new_cursor = (r.cursor + half).min(entries.len() - 1);

    if new_cursor >= entries.len() - 1 {
        return ScrollState::Tail;
    }

    let new_top = adjust_top_for_cursor(
        new_cursor,
        r.top,
        entries,
        viewport_height,
        width,
        mode,
        wrap,
        source_colors,
        source_labels,
    );

    ScrollState::pinned(entries, new_cursor, new_top)
}

/// Jump backward by half a viewport.
#[allow(clippy::too_many_arguments)]
pub fn scroll_up_half_page(
    scroll: &ScrollState,
    entries: &[LogEntry],
    viewport_height: u16,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
    source_labels: &HashMap<TaskId, String>,
) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }

    let (cursor, top) = match *scroll {
        ScrollState::Tail => (
            entries.len() - 1,
            compute_top_for_bottom(
                entries.len() - 1,
                entries,
                viewport_height,
                width,
                mode,
                wrap,
                source_colors,
                source_labels,
            ),
        ),
        ScrollState::Pinned { .. } => {
            let r = scroll.resolve(entries).expect("entries non-empty");
            (r.cursor, r.top)
        }
    };

    let half = (viewport_height / 2).max(1) as usize;
    let new_cursor = cursor.saturating_sub(half);

    let new_top = adjust_top_for_cursor(
        new_cursor,
        top,
        entries,
        viewport_height,
        width,
        mode,
        wrap,
        source_colors,
        source_labels,
    );

    ScrollState::pinned(entries, new_cursor, new_top)
}

/// Jump to first entry.
pub fn scroll_to_top(_scroll: &ScrollState, entries: &[LogEntry]) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }
    ScrollState::pinned(entries, 0, 0)
}

/// Jump to last entry (tail mode).
pub fn scroll_to_bottom(_scroll: &ScrollState, entries: &[LogEntry]) -> ScrollState {
    if entries.is_empty() {
        return ScrollState::Tail;
    }
    ScrollState::Tail
}

/// Number of entries with seq strictly greater than the pinned cursor's seq.
/// Returns 0 for `Tail` (always tracking the latest). Counts entries that
/// arrived/became visible after the pin, regardless of whether the pinned
/// entry itself is still in `entries`.
pub fn new_entries_since_pin(scroll: &ScrollState, entries: &[LogEntry]) -> usize {
    match *scroll {
        ScrollState::Tail => 0,
        ScrollState::Pinned { cursor_seq, .. } => {
            let pos = entries.partition_point(|e| e.seq <= cursor_seq);
            entries.len() - pos
        }
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
    source_labels: &HashMap<TaskId, String>,
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
            let (_, h) = render_entry(entry, width, mode, wrap, source_colors, None, source_labels);
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
            None,
            source_labels,
        );
        if y + cursor_h + margin > vh {
            // Need to scroll down: recompute top
            // Walk backward from cursor to find new top
            let mut space = margin + cursor_h;
            let mut new_top = cursor;
            while new_top > 0 && space < vh {
                new_top -= 1;
                let (_, h) = render_entry(
                    &entries[new_top],
                    width,
                    mode,
                    wrap,
                    source_colors,
                    None,
                    source_labels,
                );
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

    fn tid(name: &str) -> crate::execution::TaskId {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        crate::execution::TaskId(h.finish())
    }

    fn make_entry(raw: &str, source: &str, seq: u64) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: tid(source),
            seq,
            timestamp: None,
            level: Some("info".to_string()),
            message: Some(raw.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    fn make_entries(n: usize) -> Vec<LogEntry> {
        (0..n)
            .map(|i| make_entry(&format!("entry {}", i), "test", (i as u64) + 1))
            .collect()
    }

    #[test]
    fn layout_empty() {
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let result = layout(
            &ScrollState::Tail,
            &[],
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            None,
            true,
            &labels,
        );
        assert!(result.entries.is_empty());
    }

    #[test]
    fn layout_tail_single() {
        let entries = make_entries(1);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let result = layout(
            &ScrollState::Tail,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            None,
            true,
            &labels,
        );
        assert_eq!(result.entries.len(), 1);
        assert!(result.entries[0].is_cursor);
    }

    #[test]
    fn layout_tail_fills_viewport() {
        let entries = make_entries(30);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let result = layout(
            &ScrollState::Tail,
            &entries,
            10,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            None,
            true,
            &labels,
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
        let labels = HashMap::new();
        let state = ScrollState::pinned(&entries, 5, 0);
        let next = scroll_down(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            &labels,
        );
        assert_eq!(next.cursor_index(&entries), Some(6));
    }

    #[test]
    fn scroll_down_at_end_goes_tail() {
        let entries = make_entries(10);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let state = ScrollState::pinned(&entries, 9, 0);
        let next = scroll_down(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            &labels,
        );
        assert_eq!(next, ScrollState::Tail);
    }

    #[test]
    fn scroll_up_moves_cursor() {
        let entries = make_entries(20);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let state = ScrollState::pinned(&entries, 10, 5);
        let next = scroll_up(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            &labels,
        );
        assert_eq!(next.cursor_index(&entries), Some(9));
    }

    #[test]
    fn scroll_up_from_tail() {
        let entries = make_entries(20);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let next = scroll_up(
            &ScrollState::Tail,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            &labels,
        );
        assert_eq!(next.cursor_index(&entries), Some(18));
    }

    #[test]
    fn scroll_up_at_top_stays() {
        let entries = make_entries(10);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let state = ScrollState::pinned(&entries, 0, 0);
        let next = scroll_up(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            &labels,
        );
        assert_eq!(next, ScrollState::pinned(&entries, 0, 0));
    }

    #[test]
    fn scroll_to_top_works() {
        let entries = make_entries(20);
        let next = scroll_to_top(&ScrollState::pinned(&entries, 15, 10), &entries);
        assert_eq!(next, ScrollState::pinned(&entries, 0, 0));
    }

    #[test]
    fn scroll_to_bottom_works() {
        let entries = make_entries(20);
        let next = scroll_to_bottom(&ScrollState::pinned(&entries, 5, 0), &entries);
        assert_eq!(next, ScrollState::Tail);
    }

    #[test]
    fn cursor_highlight_in_layout() {
        let entries = make_entries(10);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let state = ScrollState::pinned(&entries, 3, 0);
        let result = layout(
            &state,
            &entries,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            None,
            true,
            &labels,
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
    fn pinned_survives_filter_shrink() {
        // Regression test for the "filter collapses to one result" bug.
        // User scrolls deep into a long log, then applies a filter that
        // narrows the visible set to fewer entries than the cursor's index.
        // The viewport must still render multiple entries, with the cursor
        // landing on the nearest surviving entry.
        let full = make_entries(50);
        let pinned_at_50 = ScrollState::Pinned {
            cursor_seq: 50,
            top_seq: 45,
        };

        // Filter narrows to first 8 entries (seqs 1..=8) — cursor's seq 50
        // is filtered out.
        let filtered: Vec<LogEntry> = full.iter().take(8).cloned().collect();

        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let result = layout(
            &pinned_at_50,
            &filtered,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            None,
            true,
            &labels,
        );

        // Must render multiple entries, not just the last one.
        assert!(
            result.entries.len() > 1,
            "expected viewport to render multiple entries after filter shrink, got {}",
            result.entries.len()
        );
        // Cursor falls back to the last surviving entry (seq 8 → index 7).
        let cursor_entry = result
            .entries
            .iter()
            .find(|e| e.is_cursor)
            .expect("a cursor entry");
        assert_eq!(cursor_entry.entry_index, 7);
    }

    #[test]
    fn pinned_preserves_focus_when_entry_survives() {
        // If the pinned entry survives the filter, the cursor stays on it
        // even though its visible-list index has shifted.
        let full = make_entries(50);
        // Pin to entry with seq 10 (index 9 in `full`).
        let pinned = ScrollState::Pinned {
            cursor_seq: 10,
            top_seq: 8,
        };

        // Filter to even-seq entries: [seq 2, 4, 6, 8, 10, 12, ...]. Pinned
        // entry (seq 10) is at visible-index 4, not 9.
        let filtered: Vec<LogEntry> = full.iter().filter(|e| e.seq % 2 == 0).cloned().collect();

        let mut sc = SourceColors::new();
        let labels = HashMap::new();
        let result = layout(
            &pinned,
            &filtered,
            24,
            80,
            DisplayMode::Preview,
            false,
            &mut sc,
            None,
            true,
            &labels,
        );

        let cursor_entry = result
            .entries
            .iter()
            .find(|e| e.is_cursor)
            .expect("a cursor entry");
        // entry_index is the index into `filtered`; the pinned entry is at
        // visible-index 4 there.
        assert_eq!(cursor_entry.entry_index, 4);
        assert_eq!(filtered[cursor_entry.entry_index].seq, 10);
    }

    #[test]
    fn margin_adjusts_top() {
        let entries = make_entries(30);
        let mut sc = SourceColors::new();
        let labels = HashMap::new();
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
            &labels,
        );
        // top should adjust so cursor is visible with margin
        assert!(top > 0);
        assert!(15 >= top + SCROLL_MARGIN as usize);
        assert!(15 + SCROLL_MARGIN as usize <= top + 10);
    }

    #[test]
    fn new_entries_since_pin_tail() {
        let entries = make_entries(100);
        assert_eq!(new_entries_since_pin(&ScrollState::Tail, &entries), 0);
    }

    #[test]
    fn new_entries_since_pin_pinned() {
        let entries = make_entries(100);
        // Pin at index 50 (seq 51); 49 entries follow it.
        let state = ScrollState::pinned(&entries, 50, 40);
        assert_eq!(new_entries_since_pin(&state, &entries), 49);
    }

    #[test]
    fn new_entries_since_pin_when_pinned_entry_filtered_out() {
        // If the pinned entry's seq is no longer in `entries`, count the
        // entries with strictly greater seq — not "everything after the
        // resolved nearest match", which would be wrong.
        let mut entries = make_entries(10);
        // Remove the entry with seq 5.
        entries.retain(|e| e.seq != 5);
        let state = ScrollState::Pinned {
            cursor_seq: 5,
            top_seq: 5,
        };
        // Entries with seq > 5: 6, 7, 8, 9, 10 → 5
        assert_eq!(new_entries_since_pin(&state, &entries), 5);
    }
}
