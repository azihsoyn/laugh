use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::files_view::FilesView;
use crate::term::{Term, with_terminal};
use crate::threads_view::ThreadsView;
use crate::{theme, ui};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Files,
    Threads,
}

pub struct PrHeader {
    pub repo: String,
    pub number: u64,
    pub title: String,
}

/// One pull request, one TUI: the changed files and the review threads are
/// two screens of the same app, switched with 1 / 2.
struct PrApp {
    header: PrHeader,
    screen: Screen,
    help: bool,
    /// Where each screen tab was last drawn, for mouse clicks.
    tab_hits: Vec<(Screen, Rect)>,
    files: FilesView,
    threads: ThreadsView,
}

pub fn run(header: PrHeader, files: FilesView, threads: ThreadsView) -> Result<()> {
    let mut app = PrApp {
        header,
        screen: Screen::Files,
        help: false,
        tab_hits: Vec::new(),
        files,
        threads,
    };
    with_terminal(|terminal| {
        let result = event_loop(terminal, &mut app);
        if app.files.pending() > 0 {
            app.screen = Screen::Files;
            app.help = false;
            app.files.show_saving();
            terminal.draw(|f| draw(f, &mut app))?;
        }
        let saved = app.files.finish();
        result.and(saved)
    })
}

fn event_loop(terminal: &mut Term, app: &mut PrApp) -> Result<()> {
    loop {
        app.files.tick();
        terminal.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => key,
            Event::Mouse(mouse) => {
                handle_mouse(app, mouse.kind, Position::new(mouse.column, mouse.row));
                continue;
            }
            _ => continue,
        };
        if app.help {
            app.help = false;
            if !matches!(key.code, KeyCode::Char('q')) {
                continue;
            }
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('?') => app.help = true,
            KeyCode::Char('1') => app.screen = Screen::Files,
            KeyCode::Char('2') => app.screen = Screen::Threads,
            code => match app.screen {
                Screen::Files => app.files.handle_key(code),
                Screen::Threads => app.threads.handle_key(code),
            },
        }
    }
}

fn tab_at(hits: &[(Screen, Rect)], at: Position) -> Option<Screen> {
    hits.iter()
        .find(|(_, rect)| rect.contains(at))
        .map(|(screen, _)| *screen)
}

/// Clicking a screen tab switches to it; the wheel moves like ↑ / ↓ on
/// whichever screen is open (with mouse capture on, the terminal no longer
/// turns the wheel into arrow keys by itself).
fn handle_mouse(app: &mut PrApp, kind: MouseEventKind, at: Position) {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if app.help {
                app.help = false;
            } else if let Some(screen) = tab_at(&app.tab_hits, at) {
                app.screen = screen;
            }
        }
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if !app.help => {
            let code = if kind == MouseEventKind::ScrollDown {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            match app.screen {
                Screen::Files => app.files.handle_key(code),
                Screen::Threads => app.threads.handle_key(code),
            }
        }
        _ => {}
    }
}

fn draw(f: &mut ratatui::Frame<'_>, app: &mut PrApp) {
    let [top, tabs, gap, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_top(f, top, &app.header);
    draw_tabs(f, tabs, app);
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(gap.width as usize), theme::faint())),
        gap,
    );
    match app.screen {
        Screen::Files => app.files.draw(f, body),
        Screen::Threads => app.threads.draw(f, body),
    }
    draw_footer(f, footer, app);

    if app.help {
        draw_help(f, f.area());
    }
}

fn draw_top(f: &mut ratatui::Frame<'_>, area: Rect, header: &PrHeader) {
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ◆ laugh ", theme::bold(Style::default().fg(theme::BRAND))),
            Span::styled(format!(" {} ", header.repo), theme::muted()),
            Span::styled(format!("#{} ", header.number), theme::bold(theme::accent())),
            Span::styled(header.title.clone(), theme::bold(theme::text())),
        ])),
        area,
    );
}

fn draw_tabs(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut PrApp) {
    let (viewed, total) = app.files.viewed_counts();
    let tabs = [
        (Screen::Files, "1", "Files", format!("{viewed}/{total}")),
        (
            Screen::Threads,
            "2",
            "Threads",
            app.threads.len().to_string(),
        ),
    ];
    let mut spans = vec![Span::raw(" ")];
    let mut x = area.x + 1;
    app.tab_hits.clear();
    for (screen, key, name, count) in tabs {
        let on = screen == app.screen;
        let width = (format!(" {key} {name} {count} ").chars().count()) as u16;
        app.tab_hits
            .push((screen, Rect::new(x, area.y, width, 1).intersection(area)));
        x += width + 2;
        let (key_style, name_style, count_style) = if on {
            (
                theme::pill_on(),
                theme::pill_on(),
                Style::default().fg(theme::ON_ACCENT).bg(theme::ACCENT),
            )
        } else {
            (theme::faint(), theme::muted(), theme::faint())
        };
        spans.push(Span::styled(format!(" {key} "), key_style));
        spans.push(Span::styled(format!("{name} "), name_style));
        spans.push(Span::styled(format!("{count} "), count_style));
        spans.push(Span::raw("  "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut ratatui::Frame<'_>, area: Rect, app: &PrApp) {
    let mut hints = match app.screen {
        Screen::Files => app.files.hints(),
        Screen::Threads => app.threads.hints(),
    };
    hints.extend([("?", "keys"), ("q", "quit")]);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(ui::key_hints(&hints));

    let note = match app.files.status() {
        Some(status) => Some(Span::styled(
            format!("{status} "),
            Style::default().fg(theme::YELLOW),
        )),
        None if app.files.pending() > 0 => Some(Span::styled(
            format!("⇅ saving {} ", app.files.pending()),
            theme::muted(),
        )),
        None => None,
    };
    match note {
        Some(note) => {
            let width = note.content.chars().count() as u16 + 1;
            let [left, right] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(area);
            f.render_widget(Paragraph::new(Line::from(spans)), left);
            f.render_widget(Paragraph::new(Line::from(note)).right_aligned(), right);
        }
        None => f.render_widget(Paragraph::new(Line::from(spans)), area),
    }
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Anywhere",
        &[
            ("1  2", "files / threads"),
            ("?", "this list"),
            ("q", "quit (waits for pending saves)"),
        ],
    ),
    (
        "Files",
        &[
            ("j  k", "move"),
            ("h  l  ⏎", "fold, unfold, step out"),
            ("v", "viewed: this file"),
            ("V", "viewed: everything in the folder"),
            ("H", "hide / show viewed files"),
            ("g  G", "top / bottom"),
        ],
    ),
    (
        "Threads",
        &[
            ("h  l", "previous / next thread"),
            ("j  k", "scroll"),
            ("space", "scroll the thread or the code"),
            ("tab", "next person"),
            ("r", "hide / show resolved"),
            ("f", "open → resolved → outdated → all"),
        ],
    ),
];

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect) {
    let mut lines = Vec::new();
    for (i, (section, keys)) in HELP.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            *section,
            theme::bold(theme::accent()),
        )));
        for (key, what) in keys.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<10}"), theme::bold(theme::text())),
                Span::styled(*what, theme::muted()),
            ]));
        }
    }
    let height = lines.len() as u16 + 2;
    let popup = ui::centered(area, 52, height);
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(ui::panel("Keys", true)), popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_land_on_the_tab_under_the_pointer() {
        let hits = [
            (Screen::Files, Rect::new(1, 1, 14, 1)),
            (Screen::Threads, Rect::new(17, 1, 13, 1)),
        ];
        assert!(tab_at(&hits, Position::new(1, 1)) == Some(Screen::Files));
        assert!(tab_at(&hits, Position::new(20, 1)) == Some(Screen::Threads));
        assert!(tab_at(&hits, Position::new(15, 1)).is_none());
        assert!(tab_at(&hits, Position::new(20, 2)).is_none());
    }
}
