//! Centralized color theme.
//!
//! All UI colors flow through the global `THEME` static, which can be
//! overridden at startup via `THEME.set(...)` or left as the default.
//!
//! `SourceColors` lives here because it reads from `THEME.source_palette`
//! and is used by both TUI and CLI rendering.

use ratatui::style::Color;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::execution::TaskId;

// ---------------------------------------------------------------------------
// Global access
// ---------------------------------------------------------------------------

pub struct ThemeCell(OnceLock<Theme>);

impl ThemeCell {
    pub fn set(&self, theme: Theme) -> Result<(), Theme> {
        self.0.set(theme)
    }
}

impl std::ops::Deref for ThemeCell {
    type Target = Theme;
    fn deref(&self) -> &Theme {
        self.0.get_or_init(Theme::default)
    }
}

pub static THEME: ThemeCell = ThemeCell(OnceLock::new());

// ---------------------------------------------------------------------------
// Theme definition
// ---------------------------------------------------------------------------

pub struct Theme {
    // Log levels
    pub level_error: Color,
    pub level_warn: Color,
    pub level_info: Color,
    pub level_debug: Color,

    // Task/process status
    pub status_running: Color,
    pub status_failed: Color,
    pub status_stopped: Color,
    pub status_done: Color,
    pub status_setup: Color,

    // UI chrome
    pub accent: Color,
    pub dim: Color,
    pub border: Color,
    pub selection_bg: Color,
    pub selection_bg_dim: Color,

    // Search highlighting
    pub search_match_bg: Color,
    pub search_match_fg: Color,

    // Source palette (cycled for multi-source logs)
    pub source_palette: &'static [Color],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            level_error: Color::Red,
            level_warn: Color::Yellow,
            level_info: Color::Green,
            level_debug: Color::DarkGray,

            status_running: Color::Green,
            status_failed: Color::Red,
            status_stopped: Color::Yellow,
            status_done: Color::DarkGray,
            status_setup: Color::Yellow,

            accent: Color::Cyan,
            dim: Color::DarkGray,
            border: Color::DarkGray,
            selection_bg: Color::DarkGray,
            selection_bg_dim: Color::Rgb(64, 64, 64),

            search_match_bg: Color::Yellow,
            search_match_fg: Color::Black,

            source_palette: &[
                Color::Cyan,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Magenta,
                Color::Red,
            ],
        }
    }
}

impl Theme {
    /// Map a log level string to its theme color.
    pub fn level_color(&self, level: &Option<String>) -> Color {
        match level.as_deref() {
            Some("error") => self.level_error,
            Some("warn") => self.level_warn,
            Some("info") => self.level_info,
            Some("debug") | Some("trace") => self.level_debug,
            Some(_) => Color::White,
            None => self.dim,
        }
    }
}

// ---------------------------------------------------------------------------
// SourceColors — per-source color assignment from the theme palette
// ---------------------------------------------------------------------------

/// Manages source-to-color assignment, cycling through the theme's palette.
#[derive(Debug, Clone)]
pub struct SourceColors {
    map: HashMap<TaskId, Color>,
}

impl SourceColors {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Get or assign a color for the given source id.
    pub fn color_for(&mut self, source: TaskId) -> Color {
        if let Some(&color) = self.map.get(&source) {
            return color;
        }
        let palette = THEME.source_palette;
        let idx = self.map.len() % palette.len();
        let color = palette[idx];
        self.map.insert(source, color);
        color
    }
}

impl Default for SourceColors {
    fn default() -> Self {
        Self::new()
    }
}
