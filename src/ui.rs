//! Small drawing helpers shared by both screens.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Time since laugh started; drives the animations.
pub fn clock() -> std::time::Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// A spinner frame for something still loading. The UI redraws every
/// tenth of a second, so it turns while anything is waiting.
pub fn spinner() -> char {
    SPINNER[(clock().as_millis() / 100) as usize % SPINNER.len()]
}

/// A rounded panel; the border lights up in the accent colour when focused.
pub fn panel(title: &str, focused: bool) -> Block<'static> {
    let (border, title_style) = if focused {
        (theme::accent(), theme::bold(theme::accent()))
    } else {
        (theme::faint(), theme::muted())
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), title_style))
        .padding(Padding::horizontal(1))
}

/// `key label` pairs, keys drawn as small badges so they scan at a glance.
pub fn key_hints(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(theme::TEXT).bg(theme::SELECTION),
        ));
        spans.push(Span::styled(format!(" {label}"), theme::muted()));
    }
    spans
}

/// A thin progress bar: `━━━━━────` in green over a faint track.
pub fn gauge(done: usize, total: usize, width: usize) -> Vec<Span<'static>> {
    let filled = (done * width + total / 2).checked_div(total).unwrap_or(0);
    let filled = filled.min(width);
    vec![
        Span::styled("━".repeat(filled), Style::default().fg(theme::GREEN)),
        Span::styled("─".repeat(width - filled), theme::faint()),
    ]
}

/// A rectangle of the given size centred in `area`, for popups.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    cell
}

/// Two panes side by side, split by a divider the mouse can drag: press on
/// the borders where they meet, move, let go.
pub struct Split {
    /// The left pane's share of the width, in thousandths.
    share: Cell<u16>,
    /// Where it was last drawn.
    area: Cell<Option<Rect>>,
    /// While dragging: how far right of the pointer the divider sits.
    grab: Cell<Option<i32>>,
}

/// Neither pane is dragged narrower than this.
const MIN_PANE: u16 = 20;

impl Split {
    pub fn new(share: u16) -> Self {
        Split {
            share: Cell::new(share.clamp(1, 999)),
            area: Cell::new(None),
            grab: Cell::new(None),
        }
    }

    pub fn share(&self) -> u16 {
        self.share.get()
    }

    pub fn set_share(&self, share: u16) {
        self.share.set(share.clamp(1, 999));
    }

    fn left_width(&self, width: u16) -> u16 {
        let w = (u32::from(width) * u32::from(self.share.get()) / 1000) as u16;
        if width >= 2 * MIN_PANE {
            w.clamp(MIN_PANE, width - MIN_PANE)
        } else {
            w
        }
    }

    /// Lays `area` out as the two panes, and remembers it for the mouse.
    pub fn areas(&self, area: Rect) -> [Rect; 2] {
        self.area.set(Some(area));
        let left = self.left_width(area.width);
        Layout::horizontal([Constraint::Length(left), Constraint::Min(0)]).areas(area)
    }

    /// Not drawn this frame (the second pane is closed).
    pub fn hide(&self) {
        self.area.set(None);
        self.grab.set(None);
    }

    /// The column the left pane ends at (exclusive).
    fn divider(&self, area: Rect) -> u16 {
        area.x + self.left_width(area.width)
    }

    /// A press: starts a drag when it lands on the divider — the left
    /// pane's right border or the right pane's left one.
    pub fn press(&self, at: Position) -> bool {
        let Some(area) = self.area.get().filter(|a| a.contains(at)) else {
            return false;
        };
        let divider = self.divider(area);
        if at.x + 1 == divider || at.x == divider {
            self.grab.set(Some(i32::from(divider) - i32::from(at.x)));
            true
        } else {
            false
        }
    }

    /// The pointer moved with the button held: moves the divider with it.
    pub fn drag(&self, at: Position) -> bool {
        let (Some(grab), Some(area)) = (self.grab.get(), self.area.get()) else {
            return false;
        };
        if area.width < 2 * MIN_PANE {
            return true;
        }
        let left = (i32::from(at.x) + grab - i32::from(area.x))
            .clamp(i32::from(MIN_PANE), i32::from(area.width - MIN_PANE));
        // Round up so the width comes back exactly from the share.
        let share = ((left as u32 * 1000).div_ceil(u32::from(area.width))) as u16;
        self.set_share(share);
        true
    }

    /// The button came up: true if a drag just ended.
    pub fn release(&self) -> bool {
        self.grab.take().is_some()
    }
}

// ---- time ----

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the epoch for GitHub's `2026-09-04T12:07:55Z` timestamps.
fn parse_utc(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(n(0..4)?, n(5..7)?, n(8..10)?);
    Some(days * 86_400 + n(11..13)? * 3_600 + n(14..16)? * 60 + n(17..19)?)
}

fn ago(seconds: i64) -> String {
    let (minute, hour, day) = (60, 3_600, 86_400);
    match seconds {
        s if s < minute => "just now".to_string(),
        s if s < hour => format!("{}m ago", s / minute),
        s if s < day => format!("{}h ago", s / hour),
        s if s < 30 * day => format!("{}d ago", s / day),
        s if s < 365 * day => format!("{}mo ago", s / (30 * day)),
        s => format!("{}y ago", s / (365 * day)),
    }
}

/// "3d ago" for a GitHub timestamp; the raw string if it doesn't parse.
pub fn relative_time(timestamp: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match parse_utc(timestamp) {
        Some(t) => ago((now - t).max(0)),
        None => timestamp.to_string(),
    }
}

// ---- markdown, just enough of it ----

/// `**bold**` and `` `code` `` inside one line.
fn inline(line: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let style = |bold: bool| {
        if bold {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        if rest.starts_with("**") {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style(bold)));
            }
            bold = !bold;
            i += 2;
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style(bold)));
            }
            spans.push(Span::styled(
                after[..end].to_string(),
                Style::default().fg(theme::CODE),
            ));
            i += end + 2;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty");
        buf.push(ch);
        i += ch.len_utf8();
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style(bold)));
    }
    spans
}

/// Renders a (cleaned) comment body: fenced code, `diff` fences coloured by
/// +/-, quotes, headings, the `▸ Label:` sections `format::clean_body`
/// unfolds from `<details>`, and inline bold / code.
pub fn markdown(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut fence: Option<String> = None;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if let Some(lang) = trimmed.strip_prefix("```") {
            fence = match fence {
                Some(_) => None,
                None => Some(lang.trim().to_string()),
            };
            continue;
        }
        if let Some(lang) = &fence {
            let color = match (lang.as_str(), raw.chars().next()) {
                ("diff", Some('+')) => theme::GREEN,
                ("diff", Some('-')) => theme::RED,
                _ => theme::CODE,
            };
            lines.push(Line::from(Span::styled(
                format!("  {raw}"),
                Style::default().fg(color),
            )));
        } else if trimmed.starts_with("▸ ") {
            lines.push(Line::from(Span::styled(
                trimmed.to_string(),
                theme::bold(theme::accent()),
            )));
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            let mut spans = vec![Span::styled("┃ ", theme::faint())];
            spans.extend(inline(
                quote.trim_start(),
                theme::muted().add_modifier(Modifier::ITALIC),
            ));
            lines.push(Line::from(spans));
        } else if trimmed.starts_with('#') {
            lines.push(Line::from(inline(
                trimmed.trim_start_matches('#').trim_start(),
                theme::bold(theme::text()),
            )));
        } else {
            lines.push(Line::from(inline(raw, theme::text())));
        }
    }
    lines
}

/// Breaks a styled line into lines at most `width` columns wide: at the
/// last space, or between full-width characters (Japanese has no spaces
/// between words), or anywhere if a word is longer than the line. Styles
/// carry over; the space a line breaks at is dropped.
pub fn wrap(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let wide = |c: char| c.width().unwrap_or(0) > 1;
    let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut used = 0;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            let row = rows.last_mut().expect("a row");
            if used + w > width && !row.is_empty() {
                if ch == ' ' {
                    rows.push(Vec::new());
                    used = 0;
                    continue;
                }
                // Split before row[at]: after a space, or next to a
                // full-width character; right here counts too.
                let here = wide(ch) || row.last().is_some_and(|&(c, _)| wide(c) || c == ' ');
                let at = if here {
                    Some(row.len())
                } else {
                    (1..row.len())
                        .rev()
                        .find(|&i| row[i - 1].0 == ' ' || wide(row[i - 1].0) || wide(row[i].0))
                };
                let carried = match at {
                    Some(at) => {
                        let rest = row.split_off(at);
                        if row.last().is_some_and(|&(c, _)| c == ' ') {
                            row.pop();
                        }
                        rest
                    }
                    None => Vec::new(),
                };
                used = carried.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
                rows.push(carried);
            }
            rows.last_mut().expect("a row").push((ch, style));
            used += w;
        }
    }
    rows.into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (ch, style) in row {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(ch),
                    _ => spans.push(Span::styled(ch.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// Parses `@@ -769,8 +800,16 @@` into the old and new starting lines.
pub fn hunk_start(header: &str) -> Option<(i64, i64)> {
    let mut parts = header.split_whitespace().skip(1);
    let number = |p: &str| p[1..].split(',').next()?.parse::<i64>().ok();
    let old = number(parts.next()?)?;
    let new = number(parts.next()?)?;
    Some((old, new))
}

/// A unified diff (one hunk or a whole file's patch) with a gutter of new
/// line numbers and added / removed lines tinted, padded to `width` so the
/// tints run the full width of the pane. Returns the lines and each one's
/// new line number, where it has one.
pub fn diff_lines(diff: &str, width: usize) -> (Vec<Line<'static>>, Vec<Option<i64>>) {
    let mut new = 0;
    let mut lines = Vec::new();
    let mut numbered = Vec::new();
    for raw in diff.lines() {
        if raw.starts_with("@@") {
            if let Some((_, n)) = hunk_start(raw) {
                new = n;
            }
            lines.push(Line::from(Span::styled(raw.to_string(), theme::faint())));
            numbered.push(None);
            continue;
        }
        if raw.starts_with('\\') {
            // "\ No newline at end of file"
            lines.push(Line::from(Span::styled(
                format!("       {raw}"),
                theme::faint(),
            )));
            numbered.push(None);
            continue;
        }
        let (marker, rest) = raw.split_at(raw.chars().next().map_or(0, char::len_utf8));
        let (number, bg, marker_color) = match marker {
            "+" => {
                new += 1;
                (Some(new - 1), Some(theme::ADDED_BG), theme::GREEN)
            }
            "-" => (None, Some(theme::REMOVED_BG), theme::RED),
            _ => {
                new += 1;
                (Some(new - 1), None, theme::FAINT)
            }
        };
        let gutter = number.map_or("     ".to_string(), |n| format!("{n:>4} "));
        let used = gutter.width() + 2 + rest.width();
        let pad = " ".repeat(width.saturating_sub(used));
        let line = Line::from(vec![
            Span::styled(gutter, theme::faint()),
            Span::styled(format!("{marker} "), Style::default().fg(marker_color)),
            Span::styled(rest.to_string(), theme::text()),
            Span::raw(pad),
        ]);
        lines.push(match bg {
            Some(bg) => line.style(Style::default().bg(bg)),
            None => line,
        });
        numbered.push(number);
    }
    (lines, numbered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_the_divider_moves_it_with_the_pointer() {
        let split = Split::new(400);
        let [left, right] = split.areas(Rect::new(0, 5, 100, 20));
        assert_eq!((left.width, right.x), (40, 40));
        // Away from the divider nothing happens.
        assert!(!split.press(Position::new(10, 8)));
        assert!(!split.drag(Position::new(70, 8)));
        // Grabbing the left pane's border and moving right 20 columns.
        assert!(split.press(Position::new(39, 8)));
        assert!(split.drag(Position::new(59, 8)));
        assert!(split.release());
        let [left, _] = split.areas(Rect::new(0, 5, 100, 20));
        assert_eq!(left.width, 60);
        // Neither side gets narrower than MIN_PANE.
        assert!(split.press(Position::new(60, 8)));
        split.drag(Position::new(99, 8));
        split.release();
        let [left, right] = split.areas(Rect::new(0, 5, 100, 20));
        assert_eq!((left.width, right.width), (80, 20));
        assert!(!split.release());
    }

    #[test]
    fn a_hidden_split_takes_no_presses() {
        let split = Split::new(500);
        split.areas(Rect::new(0, 0, 100, 10));
        split.hide();
        assert!(!split.press(Position::new(50, 3)));
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn wrap_breaks_at_spaces_or_anywhere_and_keeps_styles() {
        let line = Line::from(vec![
            Span::raw("cap the "),
            Span::styled("retries", theme::accent()),
            Span::raw(" at three"),
        ]);
        let rows = wrap(&line, 10);
        assert_eq!(texts(&rows), ["cap the", "retries at", "three"]);
        assert_eq!(rows[1].spans[0].style, theme::accent());
        assert_eq!(
            texts(&wrap(&Line::raw("実行ログが大きい"), 6)),
            ["実行ロ", "グが大", "きい"]
        );
        assert_eq!(texts(&wrap(&Line::raw(""), 6)), [""]);
        // English words stay whole; Japanese breaks between characters.
        assert_eq!(
            texts(&wrap(&Line::raw("signal は GetObject に渡す"), 12)),
            ["signal は", "GetObject に", "渡す"]
        );
        assert_eq!(
            texts(&wrap(&Line::raw("ほかの S3 操作は再試行します"), 12)),
            ["ほかの S3 操", "作は再試行し", "ます"]
        );
    }

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn civil_dates_match_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(parse_utc("1970-01-02T00:00:10Z"), Some(86_410));
    }

    #[test]
    fn ago_picks_the_largest_sensible_unit() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(3 * 3_600), "3h ago");
        assert_eq!(ago(4 * 86_400), "4d ago");
        assert_eq!(ago(400 * 86_400), "1y ago");
    }

    #[test]
    fn inline_bold_and_code_lose_their_markers() {
        let spans = inline("use **this** not `that`", theme::text());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "use this not that");
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[3].style.fg, Some(theme::CODE));
    }

    #[test]
    fn fences_are_dropped_and_diff_lines_coloured() {
        let lines = markdown("before\n```diff\n-old\n+new\n```\nafter");
        let texts: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(texts, ["before", "  -old", "  +new", "after"]);
        assert_eq!(lines[1].spans[0].style.fg, Some(theme::RED));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme::GREEN));
    }
}
