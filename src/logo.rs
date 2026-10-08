//! The logo from `assets/logo.txt`, coloured for `--help` the same way
//! `assets/make_logo.py` colours the SVG.

use ratatui::style::Color;

use crate::theme;

const LOGO: &str = include_str!("../assets/logo.txt");
const EMBLEM: usize = 11;
const WORD: usize = 15;
const GH: usize = WORD + 9;

fn rgb(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m"),
        _ => String::new(),
    }
}

/// Plain-text logo with ANSI colours; clap strips them when colour is off.
pub fn colored() -> String {
    let lines: Vec<&str> = LOGO.trim_end().lines().collect();
    let last = lines.len() - 1;
    let mut out = String::new();
    for (row, line) in lines.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let color = match (col, ch) {
                (_, ' ') => None,
                (c, '✓') if c < EMBLEM => Some(theme::GREEN),
                (c, _) if c < EMBLEM && row == 2 && (3..=7).contains(&c) => Some(theme::BRAND),
                (c, _) if c < EMBLEM => Some(theme::ACCENT),
                _ if row == last => Some(theme::MUTED),
                (c, _) if c >= GH => Some(theme::BRAND),
                _ => Some(theme::TEXT),
            };
            match color {
                Some(color) => {
                    out.push_str(&rgb(color));
                    out.push(ch);
                    out.push_str("\x1b[0m");
                }
                None => out.push(ch),
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colouring_keeps_the_logo_text_intact() {
        let stripped: String = {
            let mut s = colored();
            while let Some(start) = s.find('\x1b') {
                let end = s[start..].find('m').map_or(s.len(), |e| start + e + 1);
                s.replace_range(start..end, "");
            }
            s
        };
        assert_eq!(stripped.trim_end(), LOGO.trim_end());
    }

    #[test]
    fn gh_is_where_the_logo_says_it_is() {
        let first = LOGO.lines().next().unwrap();
        let word: String = first.chars().skip(WORD).collect();
        assert_eq!(word.chars().count(), 15, "five three-column letters");
        assert!(LOGO.lines().nth(1).unwrap().chars().nth(GH + 1) == Some(' '));
    }
}
