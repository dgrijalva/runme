//! ANSI escape sequence utilities.
//!
//! Provides stripping of ANSI escape sequences from strings and conversion
//! of ratatui `Color` values to ANSI foreground escape codes for CLI output.

use ratatui::style::Color;

/// ANSI reset sequence — clears all attributes.
pub const RESET: &str = "\x1b[0m";

/// Return the ANSI foreground escape sequence for a ratatui `Color`.
///
/// Returns an empty string for colors that don't map to standard ANSI
/// (e.g., `Rgb`, `Indexed`).
pub fn fg(color: Color) -> &'static str {
    match color {
        Color::Black => "\x1b[30m",
        Color::Red => "\x1b[31m",
        Color::Green => "\x1b[32m",
        Color::Yellow => "\x1b[33m",
        Color::Blue => "\x1b[34m",
        Color::Magenta => "\x1b[35m",
        Color::Cyan => "\x1b[36m",
        Color::Gray => "\x1b[37m",
        Color::DarkGray => "\x1b[90m",
        Color::LightRed => "\x1b[91m",
        Color::LightGreen => "\x1b[92m",
        Color::LightYellow => "\x1b[93m",
        Color::LightBlue => "\x1b[94m",
        Color::LightMagenta => "\x1b[95m",
        Color::LightCyan => "\x1b[96m",
        Color::White => "\x1b[97m",
        Color::Reset => "\x1b[39m",
        _ => "",
    }
}

/// Strip all ANSI escape sequences from a string.
///
/// Handles CSI sequences (`ESC [ ... letter`) which cover colors, cursor
/// movement, and other terminal control. Non-CSI escape sequences (e.g.,
/// `ESC ]` for OSC) are stripped as `ESC` + one character.
pub fn strip(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Consume the escape sequence. Non-CSI sequences are skipped as
            // ESC + one char (the `chars.next()` below).
            if let Some(next) = chars.next()
                && next == '['
            {
                // CSI sequence: consume parameters until a terminal letter
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_color_codes() {
        let input = "\x1b[31mERROR\x1b[0m: something failed";
        assert_eq!(strip(input), "ERROR: something failed");
    }

    #[test]
    fn strip_handles_no_escapes() {
        let input = "plain text";
        assert_eq!(strip(input), "plain text");
    }

    #[test]
    fn strip_handles_multiple_sequences() {
        let input = "\x1b[1m\x1b[33mWARN\x1b[0m: \x1b[90mtimestamp\x1b[0m";
        assert_eq!(strip(input), "WARN: timestamp");
    }

    #[test]
    fn strip_handles_256_color() {
        let input = "\x1b[38;5;196mred\x1b[0m";
        assert_eq!(strip(input), "red");
    }

    #[test]
    fn strip_handles_rgb_color() {
        let input = "\x1b[38;2;255;0;0mred\x1b[0m";
        assert_eq!(strip(input), "red");
    }

    #[test]
    fn strip_empty_string() {
        assert_eq!(strip(""), "");
    }

    #[test]
    fn fg_standard_colors() {
        assert_eq!(fg(Color::Red), "\x1b[31m");
        assert_eq!(fg(Color::Green), "\x1b[32m");
        assert_eq!(fg(Color::DarkGray), "\x1b[90m");
        assert_eq!(fg(Color::White), "\x1b[97m");
    }
}
