//! Colours, and the span helpers every screen builds its lines from.
//!
//! Named ANSI colours rather than the mockup's hex: pacrat draws inside
//! somebody else's terminal, and a hard-coded `#D08050` overrides a palette
//! the user chose on purpose (and lies about itself on the 16-colour
//! consoles this tool is meant to survive on). The cost is that the mockup's
//! two oranges — `--t-acc` for chrome, `--t-amber` for warnings — collapse
//! onto one yellow here. That is the price of sixteen colours, and the
//! distinction the design actually needs (chrome vs. severity) is carried by
//! position and glyph, not only by hue.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Chrome: the active tab, the focused region's rule, action keys.
pub const ACCENT: Color = Color::Yellow;
/// Labels, hints, and everything that is context rather than content.
pub const DIM: Color = Color::DarkGray;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;
/// Values worth the eye landing on: hosts, package names, paths.
pub const INFO: Color = Color::Blue;

pub fn plain(s: impl Into<String>) -> Span<'static> {
    Span::raw(s.into())
}

pub fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(DIM))
}

pub fn accent(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(ACCENT))
}

pub fn tinted(colour: Color, s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(colour))
}

pub fn bold(colour: Color, s: impl Into<String>) -> Span<'static> {
    Span::styled(
        s.into(),
        Style::new().fg(colour).add_modifier(Modifier::BOLD),
    )
}
