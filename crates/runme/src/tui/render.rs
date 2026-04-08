//! Entry rendering logic for the TUI log viewer.
//!
//! Converts `LogEntry`s into styled `ratatui::text::Line`s for display,
//! supporting preview/raw display modes and truncated/wrapped line handling.

use std::collections::HashMap;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::log::LogEntry;
use crate::log::format as log_fmt;

/// Display mode for log entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Structured columns: timestamp | level | source | message
    Preview,
    /// Original text as received
    Raw,
}

// Column widths and shared helpers from the format module.
use log_fmt::{
    COLUMN_GAP_WIDTH as COLUMN_GAP, LEVEL_WIDTH, SOURCE_WIDTH, TIMESTAMP_WIDTH, pad_or_truncate,
};

/// The palette of colors assigned to sources.
const SOURCE_PALETTE: &[Color] = &[
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Red,
];

/// Manages source-to-color assignment, cycling through a palette.
#[derive(Debug, Clone)]
pub struct SourceColors {
    map: HashMap<String, Color>,
}

impl SourceColors {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Get or assign a color for the given source name.
    pub fn color_for(&mut self, source: &str) -> Color {
        if let Some(&color) = self.map.get(source) {
            return color;
        }
        let idx = self.map.len() % SOURCE_PALETTE.len();
        let color = SOURCE_PALETTE[idx];
        self.map.insert(source.to_string(), color);
        color
    }
}

impl Default for SourceColors {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a single log entry into styled lines.
///
/// Returns the styled lines and the visual height (number of lines).
pub fn render_entry(
    entry: &LogEntry,
    width: u16,
    mode: DisplayMode,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> (Vec<Line<'static>>, usize) {
    match mode {
        DisplayMode::Preview => render_preview(entry, width, wrap, source_colors),
        DisplayMode::Raw => render_raw(entry, width, wrap),
    }
}

/// Render in preview mode: structured columns.
fn render_preview(
    entry: &LogEntry,
    width: u16,
    wrap: bool,
    source_colors: &mut SourceColors,
) -> (Vec<Line<'static>>, usize) {
    let width = width as usize;

    // Timestamp column
    let ts_text = entry.display_timestamp();
    let ts_span = Span::styled(
        pad_or_truncate(&ts_text, TIMESTAMP_WIDTH),
        Style::default().fg(Color::DarkGray),
    );

    // Level column
    let (level_text, level_color) = format_level(&entry.level);
    let level_span = Span::styled(
        pad_or_truncate(&level_text, LEVEL_WIDTH),
        Style::default().fg(level_color),
    );

    // Source column
    let source_color = source_colors.color_for(&entry.source);
    let source_span = Span::styled(
        pad_or_truncate(&entry.source, SOURCE_WIDTH),
        Style::default().fg(source_color),
    );

    // Message: use extracted message, fall back to raw
    let message = entry.message.as_deref().unwrap_or(&entry.raw).to_string();

    // Calculate prefix width (columns before message)
    let prefix_width =
        TIMESTAMP_WIDTH + COLUMN_GAP + LEVEL_WIDTH + COLUMN_GAP + SOURCE_WIDTH + COLUMN_GAP;
    let gap = " ".repeat(COLUMN_GAP);

    if !wrap {
        // Truncated: single line, clip message to remaining width
        let msg_width = width.saturating_sub(prefix_width);
        let msg_text = truncate_str(&message, msg_width);
        let msg_len = msg_text.len();
        let msg_span = Span::raw(msg_text);

        // Append structured fields in dim color using remaining space
        let fields_width = msg_width.saturating_sub(msg_len + 1);
        let fields_span = if !entry.fields.is_empty() && fields_width > 3 {
            let fields_str = format_fields_inline(&entry.fields, fields_width);
            if fields_str.is_empty() {
                Span::raw("")
            } else {
                Span::styled(
                    format!(" {}", fields_str),
                    Style::default().fg(Color::DarkGray),
                )
            }
        } else {
            Span::raw("")
        };

        let line = Line::from(vec![
            ts_span,
            Span::raw(gap.clone()),
            level_span,
            Span::raw(gap.clone()),
            source_span,
            Span::raw(gap),
            msg_span,
            fields_span,
        ]);
        (vec![line], 1)
    } else {
        // Wrapped: message may span multiple lines
        let msg_width = width.saturating_sub(prefix_width);
        if msg_width == 0 {
            // Terminal too narrow for message; show prefix only
            let line = Line::from(vec![
                ts_span,
                Span::raw(gap.clone()),
                level_span,
                Span::raw(gap.clone()),
                source_span,
                Span::raw(gap),
            ]);
            return (vec![line], 1);
        }

        let msg_lines = wrap_text(&message, msg_width);
        let line_count = msg_lines.len().max(1);
        let mut result = Vec::with_capacity(line_count);

        for (i, msg_chunk) in msg_lines.iter().enumerate() {
            if i == 0 {
                // First line: full prefix + first chunk of message
                result.push(Line::from(vec![
                    ts_span.clone(),
                    Span::raw(gap.clone()),
                    level_span.clone(),
                    Span::raw(gap.clone()),
                    source_span.clone(),
                    Span::raw(gap.clone()),
                    Span::raw(msg_chunk.clone()),
                ]));
            } else {
                // Continuation lines: indented to message column
                let indent = " ".repeat(prefix_width);
                result.push(Line::from(vec![
                    Span::raw(indent),
                    Span::raw(msg_chunk.clone()),
                ]));
            }
        }

        if result.is_empty() {
            // Empty message
            let line = Line::from(vec![
                ts_span,
                Span::raw(gap.clone()),
                level_span,
                Span::raw(gap.clone()),
                source_span,
                Span::raw(gap),
            ]);
            result.push(line);
        }

        let height = result.len();
        (result, height)
    }
}

/// Render in raw mode: show the original text as-is.
fn render_raw(entry: &LogEntry, width: u16, wrap: bool) -> (Vec<Line<'static>>, usize) {
    let width = width as usize;

    if !wrap {
        // Truncated: single line
        let text = truncate_str(&entry.raw, width);
        (vec![Line::from(text)], 1)
    } else {
        // Wrapped
        let lines = wrap_text(&entry.raw, width);
        let result: Vec<Line<'static>> = if lines.is_empty() {
            vec![Line::from(String::new())]
        } else {
            lines.into_iter().map(Line::from).collect()
        };
        let h = result.len();
        (result, h)
    }
}

/// Format a level string and return (display text, color).
///
/// Uses the shared `format_level` for the text, adds TUI color.
fn format_level(level: &Option<String>) -> (String, Color) {
    let text = log_fmt::format_level(level);
    let color = match level.as_deref() {
        Some("error") => Color::Red,
        Some("warn") => Color::Yellow,
        Some("info") => Color::Green,
        Some("debug") | Some("trace") => Color::DarkGray,
        Some(_) => Color::White,
        None => Color::DarkGray,
    };
    (text, color)
}

/// Width-aware field formatting for the TUI.
///
/// Truncates the shared field output to fit within `max_width`.
fn format_fields_inline(fields: &HashMap<String, serde_json::Value>, max_width: usize) -> String {
    if max_width < 3 {
        return String::new();
    }
    let full = log_fmt::format_fields_inline(fields);
    if full.len() <= max_width {
        full
    } else if max_width >= 1 {
        full[..max_width].to_string()
    } else {
        String::new()
    }
}

fn truncate_str(s: &str, width: usize) -> String {
    if s.len() <= width {
        s.to_string()
    } else if width >= 1 {
        // Truncate without ellipsis — just clip
        s[..width].to_string()
    } else {
        String::new()
    }
}

/// Wrap text at the given width, returning one string per visual line.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    if s.is_empty() {
        return vec![String::new()];
    }

    let mut result = Vec::new();
    for line in s.split('\n') {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut remaining = line;
        while !remaining.is_empty() {
            if remaining.len() <= width {
                result.push(remaining.to_string());
                break;
            }
            result.push(remaining[..width].to_string());
            remaining = &remaining[width..];
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_entry(
        raw: &str,
        source: &str,
        level: Option<&str>,
        message: Option<&str>,
        timestamp: Option<&str>,
    ) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: crate::log::ParsedContent::PlainText,
            source: source.to_string(),
            seq: 0,
            timestamp: timestamp.map(|s| s.to_string()),
            level: level.map(|s| s.to_string()),
            message: message.map(|s| s.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    // -- SourceColors tests --

    #[test]
    fn source_colors_assigns_consistent_colors() {
        let mut sc = SourceColors::new();
        let c1 = sc.color_for("api");
        let c2 = sc.color_for("worker");
        let c3 = sc.color_for("api");
        assert_eq!(c1, c3, "same source should get same color");
        assert_ne!(c1, c2, "different sources should get different colors");
    }

    #[test]
    fn source_colors_cycles_palette() {
        let mut sc = SourceColors::new();
        let palette_len = SOURCE_PALETTE.len();
        // Assign more sources than palette colors
        for i in 0..palette_len + 2 {
            sc.color_for(&format!("source{}", i));
        }
        // The (palette_len)th source should cycle back to the first color
        let first_color = sc.color_for("source0");
        let cycled_color = sc.color_for(&format!("source{}", palette_len));
        assert_eq!(first_color, cycled_color);
    }

    // -- Preview mode, truncated --

    #[test]
    fn preview_truncated_basic() {
        let entry = make_entry(
            "raw text here",
            "api",
            Some("info"),
            Some("Hello world"),
            None,
        );
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 80, DisplayMode::Preview, false, &mut sc);
        assert_eq!(height, 1, "truncated mode should produce exactly 1 line");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn preview_truncated_clips_long_message() {
        let long_msg = "x".repeat(200);
        let entry = make_entry("raw", "api", Some("info"), Some(&long_msg), None);
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 80, DisplayMode::Preview, false, &mut sc);
        assert_eq!(height, 1);
        // The combined width should not exceed terminal width
        let total_width: usize = lines[0].spans.iter().map(|s| s.content.len()).sum();
        assert!(
            total_width <= 80,
            "line width {} exceeds terminal width 80",
            total_width
        );
    }

    #[test]
    fn preview_truncated_level_colors() {
        let mut sc = SourceColors::new();

        let error_entry = make_entry("raw", "api", Some("error"), Some("err"), None);
        let (lines, _) = render_entry(&error_entry, 80, DisplayMode::Preview, false, &mut sc);
        // Level span is the 3rd span (index 2, after ts and gap)
        let level_span = &lines[0].spans[2];
        assert_eq!(level_span.style.fg, Some(Color::Red));

        let warn_entry = make_entry("raw", "api", Some("warn"), Some("w"), None);
        let (lines, _) = render_entry(&warn_entry, 80, DisplayMode::Preview, false, &mut sc);
        let level_span = &lines[0].spans[2];
        assert_eq!(level_span.style.fg, Some(Color::Yellow));

        let info_entry = make_entry("raw", "api", Some("info"), Some("i"), None);
        let (lines, _) = render_entry(&info_entry, 80, DisplayMode::Preview, false, &mut sc);
        let level_span = &lines[0].spans[2];
        assert_eq!(level_span.style.fg, Some(Color::Green));

        let debug_entry = make_entry("raw", "api", Some("debug"), Some("d"), None);
        let (lines, _) = render_entry(&debug_entry, 80, DisplayMode::Preview, false, &mut sc);
        let level_span = &lines[0].spans[2];
        assert_eq!(level_span.style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn preview_truncated_no_level() {
        let entry = make_entry("raw", "api", None, Some("msg"), None);
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 80, DisplayMode::Preview, false, &mut sc);
        assert_eq!(height, 1);
        // Level span should show "---"
        let level_span = &lines[0].spans[2];
        assert!(level_span.content.contains("---"));
    }

    #[test]
    fn preview_truncated_falls_back_to_raw() {
        let entry = make_entry("the raw text", "api", Some("info"), None, None);
        let mut sc = SourceColors::new();
        let (lines, _) = render_entry(&entry, 100, DisplayMode::Preview, false, &mut sc);
        // Message span (index 6: after ts, gap, level, gap, source, gap) should contain raw text
        let msg_span = &lines[0].spans[6];
        assert!(msg_span.content.contains("the raw text"));
    }

    // -- Preview mode, wrapped --

    #[test]
    fn preview_wrapped_short_message() {
        let entry = make_entry("raw", "api", Some("info"), Some("short"), None);
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 80, DisplayMode::Preview, true, &mut sc);
        assert_eq!(height, 1, "short message should fit on one line");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn preview_wrapped_long_message() {
        // Prefix width = 12 + 2 + 5 + 2 + 10 + 2 = 33
        // With width=50, message gets 17 chars per line
        let msg = "a".repeat(40);
        let entry = make_entry("raw", "api", Some("info"), Some(&msg), None);
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 50, DisplayMode::Preview, true, &mut sc);
        assert!(height > 1, "long message should wrap to multiple lines");
        assert_eq!(lines.len(), height);
        // First line has prefix spans; continuation lines are indented
        assert!(
            lines[0].spans.len() > 2,
            "first line should have prefix spans"
        );
    }

    // -- Raw mode, truncated --

    #[test]
    fn raw_truncated_basic() {
        let entry = make_entry(
            "Hello world raw output",
            "api",
            Some("info"),
            Some("msg"),
            None,
        );
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 80, DisplayMode::Raw, false, &mut sc);
        assert_eq!(height, 1);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(text, "Hello world raw output");
    }

    #[test]
    fn raw_truncated_clips() {
        let raw = "x".repeat(100);
        let entry = make_entry(&raw, "api", None, None, None);
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 50, DisplayMode::Raw, false, &mut sc);
        assert_eq!(height, 1);
        let text: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(text.len(), 50);
    }

    // -- Raw mode, wrapped --

    #[test]
    fn raw_wrapped_basic() {
        let raw = "a".repeat(100);
        let entry = make_entry(&raw, "api", None, None, None);
        let mut sc = SourceColors::new();
        let (lines, height) = render_entry(&entry, 40, DisplayMode::Raw, true, &mut sc);
        assert_eq!(height, 3, "100 chars at width 40 should be 3 lines");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn raw_wrapped_multiline() {
        let raw = "line1\nline2\nline3";
        let entry = make_entry(raw, "api", None, None, None);
        let mut sc = SourceColors::new();
        let (_lines, height) = render_entry(&entry, 80, DisplayMode::Raw, true, &mut sc);
        assert_eq!(height, 3);
    }

    // -- Utility function tests --

    #[test]
    fn test_pad_or_truncate() {
        assert_eq!(pad_or_truncate("hello", 10), "hello     ");
        assert_eq!(pad_or_truncate("hello world", 5), "hello");
        assert_eq!(pad_or_truncate("exact", 5), "exact");
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
        assert_eq!(truncate_str("hi", 0), "");
    }

    #[test]
    fn test_wrap_text() {
        assert_eq!(wrap_text("hello", 10), vec!["hello"]);
        assert_eq!(wrap_text("hello world!", 5), vec!["hello", " worl", "d!"]);
        assert_eq!(wrap_text("a\nb\nc", 80), vec!["a", "b", "c"]);
        assert_eq!(wrap_text("", 80), vec![""]);
    }

    #[test]
    fn test_format_level() {
        assert_eq!(
            format_level(&Some("error".to_string())),
            ("ERROR".to_string(), Color::Red)
        );
        assert_eq!(
            format_level(&Some("warn".to_string())),
            ("WARN".to_string(), Color::Yellow)
        );
        assert_eq!(
            format_level(&Some("info".to_string())),
            ("INFO".to_string(), Color::Green)
        );
        assert_eq!(
            format_level(&Some("debug".to_string())),
            ("DEBUG".to_string(), Color::DarkGray)
        );
        assert_eq!(format_level(&None), ("---".to_string(), Color::DarkGray));
    }
}
