//! The logo from `assets/logo.txt`, coloured for `--help` the same way
//! `assets/make_logo.py` colours the SVG: a review comment smiling with
//! Viewed ticks for eyes, beside a diff that turns a frown into a laugh.

use ratatui::style::Color;

use crate::theme;

const LOGO: &str = include_str!("../assets/logo.txt");
const EMBLEM: usize = 11;
const DIFF: usize = 15;
const DEL_ROW: usize = 1;
const ADD_ROW: usize = 2;
/// Width of the tinted band behind each diff line.
const BAND: usize = 12;

struct Paint {
    fg: Color,
    bold: bool,
}

fn paint(row: usize, col: usize, ch: char, last_row: usize) -> Option<Paint> {
    let fg = |fg| Some(Paint { fg, bold: false });
    let bold = |fg| Some(Paint { fg, bold: true });
    if ch == ' ' {
        return None;
    }
    if col < EMBLEM {
        return match ch {
            '✓' => bold(theme::GREEN),
            _ if row == 2 && (3..=7).contains(&col) => fg(theme::BRAND),
            _ => fg(theme::ACCENT),
        };
    }
    if row == last_row {
        return fg(theme::MUTED);
    }
    let at = col - DIFF;
    match (row, at) {
        (_, 0..=1) => fg(theme::FAINT),
        (DEL_ROW, 3) => fg(theme::RED),
        (ADD_ROW, 3) => fg(theme::GREEN),
        (DEL_ROW, _) => fg(theme::MUTED),
        (ADD_ROW, 8..) => bold(theme::BRAND),
        _ => bold(theme::TEXT),
    }
}

fn sgr(fg: Color, bg: Option<Color>, bold: bool) -> String {
    let mut s = String::from("\x1b[");
    if bold {
        s.push_str("1;");
    }
    if let Color::Rgb(r, g, b) = fg {
        s.push_str(&format!("38;2;{r};{g};{b}"));
    }
    if let Some(Color::Rgb(r, g, b)) = bg {
        s.push_str(&format!(";48;2;{r};{g};{b}"));
    }
    s.push('m');
    s
}

/// Plain-text logo with ANSI colours; clap strips them when colour is off.
pub fn colored() -> String {
    let lines: Vec<&str> = LOGO.trim_end().lines().collect();
    let last_row = lines.len() - 1;
    let mut out = String::new();
    for (row, line) in lines.iter().enumerate() {
        let band = match row {
            DEL_ROW => Some(theme::REMOVED_BG),
            ADD_ROW => Some(theme::ADDED_BG),
            _ => None,
        };
        let chars: Vec<char> = line.chars().collect();
        let width = if band.is_some() {
            chars.len().max(DIFF + BAND)
        } else {
            chars.len()
        };
        for col in 0..width {
            let ch = chars.get(col).copied().unwrap_or(' ');
            let bg = band.filter(|_| (DIFF..DIFF + BAND).contains(&col));
            match (paint(row, col, ch, last_row), bg) {
                (Some(p), bg) => {
                    out.push_str(&sgr(p.fg, bg, p.bold));
                    out.push(ch);
                    out.push_str("\x1b[0m");
                }
                (None, Some(bg)) => {
                    out.push_str(&sgr(theme::TEXT, Some(bg), false));
                    out.push(ch);
                    out.push_str("\x1b[0m");
                }
                (None, None) => out.push(ch),
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(mut s: String) -> String {
        while let Some(start) = s.find('\x1b') {
            let end = s[start..].find('m').map_or(s.len(), |e| start + e + 1);
            s.replace_range(start..end, "");
        }
        s
    }

    #[test]
    fn colouring_keeps_the_logo_text_intact() {
        let stripped: Vec<String> = strip(colored())
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        let logo: Vec<&str> = LOGO.trim_end().lines().map(str::trim_end).collect();
        assert_eq!(stripped, logo);
    }

    #[test]
    fn the_diff_sits_where_the_painter_expects_it() {
        let row =
            |r: usize| -> String { LOGO.lines().nth(r).unwrap().chars().skip(DIFF).collect() };
        assert_eq!(row(DEL_ROW), "12 - frown");
        assert_eq!(row(ADD_ROW), "12 + laugh");
        assert!(row(ADD_ROW).len() <= BAND);
    }
}
