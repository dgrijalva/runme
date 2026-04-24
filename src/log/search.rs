use std::ops::Range;

use regex::Regex;

use super::LogEntry;

// ---------------------------------------------------------------------------
// Search result types
// ---------------------------------------------------------------------------

/// A single match within a search (no context).
#[derive(Debug)]
pub struct SearchMatch<'a> {
    /// The matching log entry.
    pub entry: &'a LogEntry,
    /// Position of this entry in the source slice.
    pub index: usize,
    /// Byte ranges within the matched text where the pattern was found.
    pub match_ranges: Vec<Range<usize>>,
}

/// Collection of search matches (no context).
#[derive(Debug)]
pub struct SearchResult<'a> {
    pub matches: Vec<SearchMatch<'a>>,
}

/// A single entry within a context group.
#[derive(Debug)]
pub struct ContextEntry<'a> {
    /// The log entry.
    pub entry: &'a LogEntry,
    /// Position of this entry in the source slice.
    pub index: usize,
    /// Whether this entry is a direct match (true) or context (false).
    pub is_match: bool,
    /// Byte ranges within the matched text. Empty if `is_match` is false.
    pub match_ranges: Vec<Range<usize>>,
}

/// A group of contiguous entries (matches + their surrounding context).
/// When context windows of adjacent matches overlap, they are merged into
/// a single group.
#[derive(Debug)]
pub struct ContextGroup<'a> {
    pub entries: Vec<ContextEntry<'a>>,
}

/// Search results with context windows. Matches that are close enough to
/// have overlapping context are merged into the same [`ContextGroup`].
#[derive(Debug)]
pub struct SearchResultWithContext<'a> {
    pub groups: Vec<ContextGroup<'a>>,
}

// ---------------------------------------------------------------------------
// Search builder
// ---------------------------------------------------------------------------

/// Configures and executes a search over a slice of [`LogEntry`].
///
/// ```ignore
/// let results = Search::new("error")
///     .case_sensitive(true)
///     .regex(true)
///     .context(3)
///     .execute(&entries);
/// ```
pub struct Search {
    pattern: String,
    case_sensitive: bool,
    is_regex: bool,
    context_lines: usize,
}

impl Search {
    /// Create a new search for the given pattern.
    pub fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
            case_sensitive: false,
            is_regex: false,
            context_lines: 0,
        }
    }

    /// Set whether the search is case-sensitive (default: false).
    pub fn case_sensitive(mut self, yes: bool) -> Self {
        self.case_sensitive = yes;
        self
    }

    /// Set whether the pattern should be treated as a regex (default: false).
    pub fn regex(mut self, yes: bool) -> Self {
        self.is_regex = yes;
        self
    }

    /// Set the number of context lines before and after each match.
    /// When 0 (the default), `execute_with_context` still works but each
    /// match group contains only the matching entry itself.
    pub fn context(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Execute the search without context, returning only matching entries.
    pub fn execute<'a>(&self, entries: &'a [LogEntry]) -> SearchResult<'a> {
        let compiled = self.compile();
        let matches = entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let ranges = compiled.find_matches(entry);
                if ranges.is_empty() {
                    None
                } else {
                    Some(SearchMatch {
                        entry,
                        index: i,
                        match_ranges: ranges,
                    })
                }
            })
            .collect();
        SearchResult { matches }
    }

    /// Execute the search with context windows.
    ///
    /// Each match gets `context_lines` entries before and after. When context
    /// windows of nearby matches overlap, they are merged into a single
    /// [`ContextGroup`].
    pub fn execute_with_context<'a>(&self, entries: &'a [LogEntry]) -> SearchResultWithContext<'a> {
        let compiled = self.compile();
        let len = entries.len();

        // First pass: find all matches and their ranges.
        let match_info: Vec<(usize, Vec<Range<usize>>)> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let ranges = compiled.find_matches(entry);
                if ranges.is_empty() {
                    None
                } else {
                    Some((i, ranges))
                }
            })
            .collect();

        if match_info.is_empty() {
            return SearchResultWithContext { groups: vec![] };
        }

        // Second pass: compute context windows and merge overlapping ones.
        // Each window is [start, end) indices into `entries`.
        let mut windows: Vec<(usize, usize)> = Vec::new();
        for (idx, _) in &match_info {
            let start = idx.saturating_sub(self.context_lines);
            let end = (*idx + self.context_lines + 1).min(len);
            if let Some(last) = windows.last_mut()
                && start <= last.1
            {
                // Overlapping or adjacent -- extend the current window.
                last.1 = end;
                continue;
            }
            windows.push((start, end));
        }

        // Build a lookup from entry index to match_ranges for fast access.
        let mut match_map: std::collections::HashMap<usize, &Vec<Range<usize>>> =
            std::collections::HashMap::new();
        for (idx, ranges) in &match_info {
            match_map.insert(*idx, ranges);
        }

        // Third pass: build ContextGroups.
        let groups = windows
            .into_iter()
            .map(|(start, end)| {
                let context_entries = (start..end)
                    .map(|i| {
                        let is_match = match_map.contains_key(&i);
                        ContextEntry {
                            entry: &entries[i],
                            index: i,
                            is_match,
                            match_ranges: if is_match {
                                match_map[&i].clone()
                            } else {
                                vec![]
                            },
                        }
                    })
                    .collect();
                ContextGroup {
                    entries: context_entries,
                }
            })
            .collect();

        SearchResultWithContext { groups }
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Compile the pattern into an internal matcher.
    fn compile(&self) -> CompiledPattern {
        if self.is_regex {
            let re = if self.case_sensitive {
                Regex::new(&self.pattern).expect("invalid regex pattern")
            } else {
                Regex::new(&format!("(?i){}", self.pattern)).expect("invalid regex pattern")
            };
            CompiledPattern::Regex(re)
        } else if self.case_sensitive {
            CompiledPattern::LiteralCaseSensitive(self.pattern.clone())
        } else {
            CompiledPattern::LiteralCaseInsensitive(self.pattern.to_lowercase())
        }
    }
}

/// Internal compiled pattern -- avoids recompiling per entry.
enum CompiledPattern {
    LiteralCaseSensitive(String),
    LiteralCaseInsensitive(String), // stored lowercase
    Regex(Regex),
}

impl CompiledPattern {
    /// Search a single entry. Returns byte ranges of all matches across the
    /// `raw` and `message` fields.
    ///
    /// Matches in `raw` are returned first. If the entry also has a `message`
    /// field, matches found there are appended. The ranges for `raw` and
    /// `message` refer to byte positions within their respective strings.
    fn find_matches(&self, entry: &LogEntry) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        self.find_in(&entry.raw, &mut ranges);
        if let Some(ref msg) = entry.message {
            self.find_in(msg, &mut ranges);
        }
        ranges
    }

    fn find_in(&self, haystack: &str, out: &mut Vec<Range<usize>>) {
        match self {
            CompiledPattern::LiteralCaseSensitive(needle) => {
                if needle.is_empty() {
                    // Empty pattern: single match at position 0 (signals "entry matched").
                    out.push(0..0);
                    return;
                }
                let mut start = 0;
                while let Some(pos) = haystack[start..].find(needle.as_str()) {
                    let abs = start + pos;
                    out.push(abs..abs + needle.len());
                    start = abs + needle.len();
                }
            }
            CompiledPattern::LiteralCaseInsensitive(needle_lower) => {
                if needle_lower.is_empty() {
                    out.push(0..0);
                    return;
                }
                let lower = haystack.to_lowercase();
                let mut start = 0;
                while let Some(pos) = lower[start..].find(needle_lower.as_str()) {
                    let abs = start + pos;
                    out.push(abs..abs + needle_lower.len());
                    start = abs + needle_lower.len();
                }
            }
            CompiledPattern::Regex(re) => {
                for m in re.find_iter(haystack) {
                    out.push(m.start()..m.end());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{LogEntry, ParsedContent};
    use std::collections::HashMap;

    /// Helper to build a minimal LogEntry for testing.
    fn entry(raw: &str, message: Option<&str>) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: "test".to_string(),
            seq: 0,
            timestamp: None,
            level: None,
            message: message.map(|s| s.to_string()),
            fields: HashMap::new(),
            stream: None,
        }
    }

    fn entry_with_source(raw: &str, source: &str, seq: u64) -> LogEntry {
        LogEntry {
            received_at: chrono::Utc::now(),
            raw: raw.to_string(),
            parsed: ParsedContent::PlainText,
            source: source.to_string(),
            seq,
            timestamp: None,
            level: None,
            message: None,
            fields: HashMap::new(),
            stream: None,
        }
    }

    // -- Basic text search --

    #[test]
    fn text_search_finds_match_in_raw() {
        let entries = vec![
            entry("INFO: starting up", None),
            entry("ERROR: disk full", None),
            entry("INFO: request handled", None),
        ];
        let result = Search::new("ERROR").execute(&entries);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].index, 1);
        assert_eq!(result.matches[0].match_ranges, vec![0..5]);
    }

    #[test]
    fn text_search_finds_match_in_message() {
        let entries = vec![entry(
            r#"{"level":"error","msg":"disk full"}"#,
            Some("disk full"),
        )];
        let result = Search::new("disk full").execute(&entries);
        assert_eq!(result.matches.len(), 1);
        // Should find ranges in both raw and message.
        assert!(!result.matches[0].match_ranges.is_empty());
    }

    #[test]
    fn text_search_case_insensitive_by_default() {
        let entries = vec![
            entry("Error: something broke", None),
            entry("nothing here", None),
        ];
        let result = Search::new("error").execute(&entries);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].index, 0);
    }

    #[test]
    fn text_search_case_sensitive() {
        let entries = vec![
            entry("Error: something broke", None),
            entry("error: also broke", None),
        ];
        let result = Search::new("error").case_sensitive(true).execute(&entries);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].index, 1);
    }

    #[test]
    fn text_search_multiple_matches_in_one_entry() {
        let entries = vec![entry("error: error again", None)];
        let result = Search::new("error").case_sensitive(true).execute(&entries);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].match_ranges, vec![0..5, 7..12]);
    }

    // -- Regex search --

    #[test]
    fn regex_search_basic() {
        let entries = vec![
            entry("2024-01-15 ERROR disk full", None),
            entry("2024-01-15 INFO started", None),
        ];
        let result = Search::new(r"ERROR|WARN").regex(true).execute(&entries);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].index, 0);
    }

    #[test]
    fn regex_search_case_insensitive() {
        let entries = vec![
            entry("Error: something", None),
            entry("WARNING: something", None),
        ];
        let result = Search::new(r"error|warning").regex(true).execute(&entries);
        assert_eq!(result.matches.len(), 2);
    }

    #[test]
    fn regex_search_case_sensitive() {
        let entries = vec![
            entry("Error: something", None),
            entry("error: something", None),
        ];
        let result = Search::new(r"^error")
            .regex(true)
            .case_sensitive(true)
            .execute(&entries);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].index, 1);
    }

    #[test]
    fn regex_search_with_capture_groups() {
        let entries = vec![entry("status=404 path=/api/users", None)];
        let result = Search::new(r"status=(\d+)")
            .regex(true)
            .case_sensitive(true)
            .execute(&entries);
        assert_eq!(result.matches.len(), 1);
        // The overall match range covers "status=404".
        assert_eq!(result.matches[0].match_ranges, vec![0..10]);
    }

    // -- Context windows --

    #[test]
    fn context_window_basic() {
        let entries: Vec<LogEntry> = (0..10).map(|i| entry(&format!("line {i}"), None)).collect();
        // Match on "line 5", context of 2
        let result = Search::new("line 5")
            .context(2)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        assert_eq!(group.entries.len(), 5); // lines 3,4,5,6,7
        assert_eq!(group.entries[0].index, 3);
        assert!(!group.entries[0].is_match);
        assert_eq!(group.entries[2].index, 5);
        assert!(group.entries[2].is_match);
        assert!(!group.entries[2].match_ranges.is_empty());
        assert_eq!(group.entries[4].index, 7);
        assert!(!group.entries[4].is_match);
    }

    #[test]
    fn context_window_at_start_of_entries() {
        let entries: Vec<LogEntry> = (0..5).map(|i| entry(&format!("line {i}"), None)).collect();
        let result = Search::new("line 0")
            .context(3)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        // Match at index 0, context 3 before (clamped to 0) and 3 after.
        assert_eq!(group.entries[0].index, 0);
        assert!(group.entries[0].is_match);
        assert_eq!(group.entries.last().unwrap().index, 3);
    }

    #[test]
    fn context_window_at_end_of_entries() {
        let entries: Vec<LogEntry> = (0..5).map(|i| entry(&format!("line {i}"), None)).collect();
        let result = Search::new("line 4")
            .context(3)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        assert_eq!(group.entries[0].index, 1);
        assert_eq!(group.entries.last().unwrap().index, 4);
        assert!(group.entries.last().unwrap().is_match);
    }

    #[test]
    fn context_window_merges_overlapping() {
        let entries: Vec<LogEntry> = (0..20).map(|i| entry(&format!("line {i}"), None)).collect();
        // Matches at index 5 and 8, context of 2.
        // Window for 5: [3, 8)  -> indices 3,4,5,6,7
        // Window for 8: [6, 11) -> indices 6,7,8,9,10
        // Merged: [3, 11)       -> indices 3..10
        let result = Search::new(r"line (5|8)")
            .regex(true)
            .case_sensitive(true)
            .context(2)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        assert_eq!(group.entries.len(), 8); // indices 3..10
        assert_eq!(group.entries.first().unwrap().index, 3);
        assert_eq!(group.entries.last().unwrap().index, 10);
        // Verify match flags.
        let match_indices: Vec<usize> = group
            .entries
            .iter()
            .filter(|e| e.is_match)
            .map(|e| e.index)
            .collect();
        assert_eq!(match_indices, vec![5, 8]);
    }

    #[test]
    fn context_window_separate_groups() {
        let entries: Vec<LogEntry> = (0..20).map(|i| entry(&format!("line {i}"), None)).collect();
        // Matches at index 2 and 15, context of 1.
        // Window for 2:  [1, 4)  -> indices 1,2,3
        // Window for 15: [14, 17) -> indices 14,15,16
        // No overlap -> two groups.
        let result = Search::new(r"line (2|15)")
            .regex(true)
            .case_sensitive(true)
            .context(1)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 2);
        assert_eq!(result.groups[0].entries.len(), 3);
        assert_eq!(result.groups[0].entries[0].index, 1);
        assert_eq!(result.groups[1].entries.len(), 3);
        assert_eq!(result.groups[1].entries[0].index, 14);
    }

    // -- Multi-source search --

    #[test]
    fn search_across_multiple_sources() {
        let entries = vec![
            entry_with_source("ERROR: auth failed", "auth-service", 1),
            entry_with_source("INFO: request ok", "api-service", 1),
            entry_with_source("ERROR: timeout", "api-service", 2),
            entry_with_source("DEBUG: retrying", "auth-service", 2),
        ];
        let result = Search::new("ERROR").execute(&entries);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[0].entry.source, "auth-service");
        assert_eq!(result.matches[1].entry.source, "api-service");
    }

    #[test]
    fn search_with_context_across_sources() {
        let entries = vec![
            entry_with_source("line A1", "a", 1),
            entry_with_source("line B1", "b", 1),
            entry_with_source("ERROR in B", "b", 2),
            entry_with_source("line A2", "a", 2),
            entry_with_source("line B3", "b", 3),
        ];
        let result = Search::new("ERROR")
            .context(1)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        // Context includes index 1 (before) and 3 (after), plus the match at 2.
        assert_eq!(group.entries.len(), 3);
        assert_eq!(group.entries[0].index, 1);
        assert!(group.entries[1].is_match);
        assert_eq!(group.entries[2].index, 3);
    }

    // -- Empty / edge cases --

    #[test]
    fn search_empty_entries() {
        let entries: Vec<LogEntry> = vec![];
        let result = Search::new("anything").execute(&entries);
        assert!(result.matches.is_empty());
    }

    #[test]
    fn search_no_matches() {
        let entries = vec![entry("hello world", None)];
        let result = Search::new("xyz").execute(&entries);
        assert!(result.matches.is_empty());
    }

    #[test]
    fn search_empty_pattern() {
        let entries = vec![entry("hello", None)];
        // An empty pattern matches everything (signals "entry matched").
        let result = Search::new("").execute(&entries);
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn context_no_matches_returns_empty_groups() {
        let entries = vec![entry("hello", None)];
        let result = Search::new("xyz").context(2).execute_with_context(&entries);
        assert!(result.groups.is_empty());
    }

    #[test]
    fn context_zero_lines_same_as_no_context() {
        let entries: Vec<LogEntry> = (0..5).map(|i| entry(&format!("line {i}"), None)).collect();
        let result = Search::new("line 2")
            .context(0)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].entries.len(), 1);
        assert!(result.groups[0].entries[0].is_match);
        assert_eq!(result.groups[0].entries[0].index, 2);
    }

    // -- Match ranges --

    #[test]
    fn match_ranges_are_correct_byte_positions() {
        let entries = vec![entry("foo bar baz", None)];
        let result = Search::new("bar").case_sensitive(true).execute(&entries);
        assert_eq!(result.matches[0].match_ranges, vec![4..7]);
    }

    #[test]
    fn match_ranges_regex() {
        let entries = vec![entry("abc 123 def 456", None)];
        let result = Search::new(r"\d+")
            .regex(true)
            .case_sensitive(true)
            .execute(&entries);
        assert_eq!(result.matches[0].match_ranges, vec![4..7, 12..15]);
    }

    #[test]
    fn match_ranges_case_insensitive() {
        let entries = vec![entry("Hello HELLO hello", None)];
        let result = Search::new("hello").execute(&entries);
        assert_eq!(result.matches[0].match_ranges.len(), 3);
    }

    // -- Context entries carry match ranges --

    #[test]
    fn context_entries_carry_match_ranges_for_matches_only() {
        let entries = vec![
            entry("before", None),
            entry("ERROR: problem", None),
            entry("after", None),
        ];
        let result = Search::new("ERROR")
            .context(1)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        // "before" -- context, no ranges
        assert!(!group.entries[0].is_match);
        assert!(group.entries[0].match_ranges.is_empty());
        // "ERROR: problem" -- match, has ranges
        assert!(group.entries[1].is_match);
        assert_eq!(group.entries[1].match_ranges, vec![0..5]);
        // "after" -- context, no ranges
        assert!(!group.entries[2].is_match);
        assert!(group.entries[2].match_ranges.is_empty());
    }

    // -- Adjacent matches merge correctly --

    #[test]
    fn adjacent_matches_merge_into_single_group() {
        let entries = vec![
            entry("ERROR one", None),
            entry("ERROR two", None),
            entry("ERROR three", None),
        ];
        let result = Search::new("ERROR")
            .context(0)
            .execute_with_context(&entries);
        // With context 0, each match window is [i, i+1).
        // Adjacent windows merge: [0,1) + [1,2) + [2,3) -> [0,3).
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].entries.len(), 3);
    }

    #[test]
    fn all_entries_match_with_large_context() {
        let entries: Vec<LogEntry> = (0..5).map(|i| entry(&format!("match {i}"), None)).collect();
        let result = Search::new("match")
            .context(10)
            .execute_with_context(&entries);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].entries.len(), 5);
        // Every entry should be marked as a match.
        for e in &result.groups[0].entries {
            assert!(e.is_match);
        }
    }
}
