//! One small palette for the whole app: a single accent, a few greys for
//! structure, and green / yellow / red only where they carry meaning.
//! Only highlights get a background; the terminal's own background shows
//! everywhere else.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(122, 162, 247);
pub const BRAND: Color = Color::Rgb(255, 158, 100);
pub const TEXT: Color = Color::Rgb(192, 202, 245);
pub const MUTED: Color = Color::Rgb(122, 130, 168);
pub const FAINT: Color = Color::Rgb(70, 76, 105);
pub const SELECTION: Color = Color::Rgb(41, 46, 66);
pub const ON_ACCENT: Color = Color::Rgb(26, 27, 38);

pub const GREEN: Color = Color::Rgb(158, 206, 106);
pub const YELLOW: Color = Color::Rgb(224, 175, 104);
pub const RED: Color = Color::Rgb(247, 118, 142);
pub const CODE: Color = Color::Rgb(125, 207, 255);

pub const ADDED_BG: Color = Color::Rgb(29, 41, 33);
pub const REMOVED_BG: Color = Color::Rgb(50, 31, 38);
pub const TARGET_BG: Color = Color::Rgb(62, 56, 34);

pub fn text() -> Style {
    Style::default().fg(TEXT)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn bold(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD)
}

pub fn selected_row() -> Style {
    Style::default().bg(SELECTION)
}

/// A filled pill, for whatever is "current" in a row of choices.
pub fn pill_on() -> Style {
    Style::default()
        .fg(ON_ACCENT)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}
