//! Shared log entry formatting.
//!
//! Produces plain-text formatted output from `LogEntry` records. Used by both
//! CLI mode (writing to stdio) and available for any non-TUI rendering.
//! The TUI has its own ratatui-native renderer that uses the same column layout.

use std::collections::HashMap;

use super::LogEntry;
use crate::ansi;
use crate::execution::TaskId;
use crate::theme::{SourceColors, THEME};

/// Build the source-column display string `[N] label` for a log entry.
///
/// Looks up the task/process label from `labels` (keyed by `TaskId`).
/// When the id has no entry, falls back to `[N] tN` so the column still
/// reflects the integer id.
pub fn format_source_label(source: TaskId, labels: &HashMap<TaskId, String>) -> String {
    let n = source.0;
    match labels.get(&source) {
        Some(name) if !name.is_empty() => format!("[{}] {}", n, name),
        _ => format!("[{}] t{}", n, n),
    }
}

/// Fixed column widths — shared between CLI formatter and TUI renderer.
pub const TIMESTAMP_WIDTH: usize = 12;
pub const LEVEL_WIDTH: usize = 5;
pub const SOURCE_WIDTH: usize = 16;
pub const COLUMN_GAP_WIDTH: usize = 2;

/// Format a LogEntry as a single structured line for CLI output.
///
/// Layout: `{timestamp}  {LEVEL}  {source}  {message} {fields}`
///
/// `labels` resolves source `TaskId`s to display labels (`[N] label`).
/// Pass an empty map to fall back to `[N] tN`.
///
/// Uses the same column layout as the TUI's preview mode.
pub fn format_entry(entry: &LogEntry, labels: &HashMap<TaskId, String>) -> String {
    let gap = " ".repeat(COLUMN_GAP_WIDTH);
    let ts = pad_or_truncate(&entry.display_timestamp(), TIMESTAMP_WIDTH);
    let level = pad_or_truncate(&format_level(&entry.level), LEVEL_WIDTH);
    let source_str = format_source_label(entry.source, labels);
    let source = pad_or_truncate(&source_str, SOURCE_WIDTH);
    let message = entry.message.as_deref().unwrap_or(&entry.raw);

    let fields_str = format_fields_inline(&entry.fields);

    if fields_str.is_empty() {
        format!("{}{}{}{}{}{}{}", ts, gap, level, gap, source, gap, message)
    } else {
        format!(
            "{}{}{}{}{}{}{} {}",
            ts, gap, level, gap, source, gap, message, fields_str
        )
    }
}

/// Format a LogEntry with ANSI colors for terminal display.
///
/// Same column layout as `format_entry`, but with theme-derived ANSI colors
/// on timestamp, level, source, and field columns.
pub fn format_entry_colored(
    entry: &LogEntry,
    source_colors: &mut SourceColors,
    labels: &HashMap<TaskId, String>,
) -> String {
    let gap = " ".repeat(COLUMN_GAP_WIDTH);
    let ts = pad_or_truncate(&entry.display_timestamp(), TIMESTAMP_WIDTH);
    let level = pad_or_truncate(&format_level(&entry.level), LEVEL_WIDTH);
    let source_str = format_source_label(entry.source, labels);
    let source = pad_or_truncate(&source_str, SOURCE_WIDTH);
    let message = entry.message.as_deref().unwrap_or(&entry.raw);

    let level_color = ansi::fg(THEME.level_color(&entry.level));
    let source_color = ansi::fg(source_colors.color_for(entry.source));
    let dim = ansi::fg(THEME.dim);
    let r = ansi::RESET;

    let fields_str = format_fields_inline(&entry.fields);

    if fields_str.is_empty() {
        format!(
            "{dim}{ts}{r}{gap}{level_color}{level}{r}{gap}{source_color}{source}{r}{gap}{message}"
        )
    } else {
        format!(
            "{dim}{ts}{r}{gap}{level_color}{level}{r}{gap}{source_color}{source}{r}{gap}{message} {dim}{fields_str}{r}"
        )
    }
}

/// Format the level string for display.
pub fn format_level(level: &Option<String>) -> String {
    match level.as_deref() {
        Some("error") => "ERROR".to_string(),
        Some("warn") => "WARN".to_string(),
        Some("info") => "INFO".to_string(),
        Some("debug") => "DEBUG".to_string(),
        Some("trace") => "TRACE".to_string(),
        Some(other) => other.to_uppercase(),
        None => "---".to_string(),
    }
}

/// Pad or truncate a string to exactly the given width.
pub fn pad_or_truncate(s: &str, width: usize) -> String {
    if s.len() >= width {
        s[..s.floor_char_boundary(width)].to_string()
    } else {
        format!("{:<width$}", s, width = width)
    }
}

/// Format structured fields as space-separated `key=value` pairs.
pub fn format_fields_inline(fields: &HashMap<String, serde_json::Value>) -> String {
    if fields.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", k, val)
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

/// Format fields sorted by interestingness score, hiding those below threshold.
///
/// Fields are sorted most-interesting first. Fields not present in `scores`
/// (e.g., when stats haven't accumulated enough data) are shown with default
/// alphabetical ordering.
pub fn format_fields_scored(
    fields: &HashMap<String, serde_json::Value>,
    scores: &HashMap<&str, f64>,
    threshold: f64,
) -> String {
    if fields.is_empty() {
        return String::new();
    }

    // If no scores available, fall back to unfiltered display.
    if scores.is_empty() {
        return format_fields_inline(fields);
    }

    let mut scored: Vec<(&String, &serde_json::Value, f64)> = fields
        .iter()
        .filter_map(|(k, v)| {
            let score = scores.get(k.as_str()).copied().unwrap_or(1.0);
            if score >= threshold {
                Some((k, v, score))
            } else {
                None
            }
        })
        .collect();

    if scored.is_empty() {
        return String::new();
    }

    // Sort by score descending, then alphabetically for ties.
    scored.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });

    scored
        .iter()
        .map(|(k, v, _)| {
            let val = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", k, val)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{LogEntry, ParsedContent};
    use std::collections::HashMap;

    #[test]
    fn test_format_entry_plain() {
        use crate::execution::TaskId;
        let entry = LogEntry::new(
            "hello world".to_string(),
            ParsedContent::PlainText,
            TaskId(42),
            0,
            None,
            Some("info".to_string()),
            Some("hello world".to_string()),
            HashMap::new(),
        );
        let labels = HashMap::new();
        let line = format_entry(&entry, &labels);
        assert!(line.contains("INFO"));
        // Fallback form when no label is registered: `[N] tN`.
        assert!(line.contains("[42] t42"), "got: {}", line);
        assert!(line.contains("hello world"));
    }

    #[test]
    fn test_format_entry_with_label() {
        use crate::execution::TaskId;
        let entry = LogEntry::new(
            "hi".to_string(),
            ParsedContent::PlainText,
            TaskId(3),
            0,
            None,
            Some("info".to_string()),
            Some("hi".to_string()),
            HashMap::new(),
        );
        let mut labels = HashMap::new();
        labels.insert(TaskId(3), "ticker".to_string());
        let line = format_entry(&entry, &labels);
        assert!(line.contains("[3] ticker"), "got: {}", line);
    }

    #[test]
    fn test_format_entry_with_fields() {
        use crate::execution::TaskId;
        let mut fields = HashMap::new();
        fields.insert("latency".to_string(), serde_json::Value::Number(42.into()));
        let entry = LogEntry::new(
            r#"{"msg":"heartbeat","latency":42}"#.to_string(),
            ParsedContent::PlainText,
            TaskId(7),
            0,
            None,
            Some("info".to_string()),
            Some("heartbeat".to_string()),
            fields,
        );
        let labels = HashMap::new();
        let line = format_entry(&entry, &labels);
        assert!(line.contains("heartbeat"));
        assert!(line.contains("latency=42"));
    }

    #[test]
    fn test_format_level_variants() {
        assert_eq!(format_level(&Some("error".into())), "ERROR");
        assert_eq!(format_level(&Some("warn".into())), "WARN");
        assert_eq!(format_level(&Some("info".into())), "INFO");
        assert_eq!(format_level(&None), "---");
    }

    #[test]
    fn test_pad_or_truncate() {
        assert_eq!(pad_or_truncate("hi", 5), "hi   ");
        assert_eq!(pad_or_truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_format_fields_empty() {
        assert_eq!(format_fields_inline(&HashMap::new()), "");
    }
}
