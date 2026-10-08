use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::checks_view::ChecksView;
use crate::files_view::{FilesView, OpenRequest};
use crate::term::{self, Term, with_terminal};
use crate::threads_view::ThreadsView;
use crate::{theme, ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Files,
    Threads,
    Checks,
}

impl Screen {
    const ALL: [Screen; 3] = [Screen::Files, Screen::Threads, Screen::Checks];

    /// The screen `delta` along from this one, wrapping around.
    fn step(self, delta: i32) -> Screen {
        let i = Screen::ALL.iter().position(|&s| s == self).unwrap_or(0) as i32;
        Screen::ALL[(i + delta).rem_euclid(Screen::ALL.len() as i32) as usize]
    }
}

pub struct PrHeader {
    pub repo: String,
    pub number: u64,
    pub title: String,
    /// Short name for the PR switcher, e.g. `infra#45`.
    pub label: String,
}

/// One TUI for one or more pull requests: the changed files and the review
/// threads are two screens (1 / 2), and with several PRs open a switcher
/// shows them all together or one at a time ([ / ]).
struct PrApp {
    prs: Vec<PrHeader>,
    /// `None` shows every open PR together; `Some(i)` just the i-th.
    scope: Option<usize>,
    screen: Screen,
    help: bool,
    /// Where each screen tab was last drawn, for mouse clicks.
    tab_hits: Vec<(Screen, Rect)>,
    /// Same for the PR switcher.
    pr_hits: Vec<(Option<usize>, Rect)>,
    files: FilesView,
    threads: ThreadsView,
    checks: ChecksView,
}

impl PrApp {
    fn set_scope(&mut self, scope: Option<usize>) {
        self.scope = scope;
        self.files.set_scope(scope);
        self.threads.set_scope(scope);
        self.checks.set_scope(scope);
    }

    /// Steps through All, then each PR, wrapping around.
    fn cycle_scope(&mut self, delta: i64) {
        let n = self.prs.len() as i64;
        if n < 2 {
            return;
        }
        let pos = self.scope.map_or(0, |i| i as i64 + 1);
        let next = (pos + delta).rem_euclid(n + 1);
        self.set_scope(if next == 0 {
            None
        } else {
            Some(next as usize - 1)
        });
    }
}

pub fn run(
    prs: Vec<PrHeader>,
    files: FilesView,
    threads: ThreadsView,
    checks: ChecksView,
) -> Result<()> {
    let mut app = PrApp {
        prs,
        scope: None,
        screen: Screen::Files,
        help: false,
        tab_hits: Vec::new(),
        pr_hits: Vec::new(),
        files,
        threads,
        checks,
    };
    if app.prs.len() == 1 {
        app.set_scope(Some(0));
    }
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
        if app.screen == Screen::Checks {
            app.checks.tick();
        }
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
        // A confirmation dialog takes every key — q and Esc cancel it
        // rather than quitting.
        if app.screen == Screen::Files && app.files.is_modal() {
            app.files.handle_key(key.code);
            continue;
        }
        // With a diff open, Esc closes it rather than quitting.
        if app.screen == Screen::Files && key.code == KeyCode::Esc && app.files.takes_esc() {
            app.files.handle_key(key.code);
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('?') => app.help = true,
            KeyCode::Char('1') => app.screen = Screen::Files,
            KeyCode::Char('2') => app.screen = Screen::Threads,
            KeyCode::Char('3') => app.screen = Screen::Checks,
            KeyCode::Tab => app.screen = app.screen.step(1),
            KeyCode::BackTab => app.screen = app.screen.step(-1),
            KeyCode::Char(']') => app.cycle_scope(1),
            KeyCode::Char('[') => app.cycle_scope(-1),
            code => match app.screen {
                Screen::Files => {
                    app.files.handle_key(code);
                    if let Some(request) = app.files.take_open_request() {
                        let status = open_externally(terminal, app, &request)?;
                        app.files.set_status(status);
                    }
                }
                Screen::Threads => app.threads.handle_key(code),
                Screen::Checks => app.checks.handle_key(code),
            },
        }
    }
}

/// Runs `LAUGH_OPEN_CMD` for a file with the terminal handed over, so a
/// pager or an editor works; one that opens somewhere else returns at once.
/// The file and its PR are in the environment, and the path is also `$1`.
fn open_externally(terminal: &mut Term, app: &PrApp, request: &OpenRequest) -> Result<String> {
    let Some(cmd) = app.files.open_cmd() else {
        return Ok(String::new());
    };
    term::suspend(terminal)?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .arg("laugh")
        .arg(&request.path)
        .env("LAUGH_FILE", &request.path)
        .env("LAUGH_REPO", format!("{}/{}", request.owner, request.repo))
        .env("LAUGH_PR", request.number.to_string())
        .env("LAUGH_BASE", &request.base)
        .env("LAUGH_HEAD", &request.head)
        .env(
            "LAUGH_URL",
            format!(
                "https://github.com/{}/{}/pull/{}/files",
                request.owner, request.repo, request.number
            ),
        )
        .status();
    term::resume(terminal)?;
    Ok(match status {
        Ok(s) if s.success() => format!("opened {}", request.path),
        Ok(s) => format!("LAUGH_OPEN_CMD exited with {}", s.code().unwrap_or(-1)),
        Err(e) => format!("couldn't run LAUGH_OPEN_CMD: {e}"),
    })
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
        _ if app.files.is_modal() => {}
        MouseEventKind::Down(MouseButton::Left) => {
            if app.help {
                app.help = false;
            } else if let Some(screen) = tab_at(&app.tab_hits, at) {
                app.screen = screen;
            } else if let Some((scope, _)) = app.pr_hits.iter().find(|(_, r)| r.contains(at)) {
                let scope = *scope;
                app.set_scope(scope);
            } else if app.screen == Screen::Files {
                app.files.click(at);
            }
        }
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if !app.help => {
            let code = if kind == MouseEventKind::ScrollDown {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            match app.screen {
                Screen::Files => app.files.wheel(kind == MouseEventKind::ScrollDown, at),
                Screen::Threads => app.threads.handle_key(code),
                Screen::Checks => app.checks.handle_key(code),
            }
        }
        _ => {}
    }
}

fn draw(f: &mut ratatui::Frame<'_>, app: &mut PrApp) {
    let switcher = if app.prs.len() > 1 { 1 } else { 0 };
    let [top, prs, tabs, gap, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(switcher),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_top(f, top, app);
    if switcher > 0 {
        draw_pr_switcher(f, prs, app);
    }
    draw_tabs(f, tabs, app);
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(gap.width as usize), theme::faint())),
        gap,
    );
    match app.screen {
        Screen::Files => app.files.draw(f, body),
        Screen::Threads => app.threads.draw(f, body),
        Screen::Checks => app.checks.draw(f, body),
    }
    draw_footer(f, footer, app);

    if app.help {
        draw_help(f, f.area());
    }
}

fn draw_top(f: &mut ratatui::Frame<'_>, area: Rect, app: &PrApp) {
    let mut spans = vec![Span::styled(
        " ◆ laugh ",
        theme::bold(Style::default().fg(theme::BRAND)),
    )];
    match app.scope {
        Some(i) => {
            let header = &app.prs[i];
            spans.push(Span::styled(format!(" {} ", header.repo), theme::muted()));
            spans.push(Span::styled(
                format!("#{} ", header.number),
                theme::bold(theme::accent()),
            ));
            spans.push(Span::styled(
                header.title.clone(),
                theme::bold(theme::text()),
            ));
        }
        None => {
            spans.push(Span::styled(
                format!(" {} pull requests", app.prs.len()),
                theme::bold(theme::text()),
            ));
            spans.push(Span::styled("  reviewed together", theme::muted()));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// `All` plus one pill per PR; records where each landed for clicks.
fn draw_pr_switcher(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut PrApp) {
    let mut choices: Vec<(Option<usize>, String)> = vec![(None, "All".to_string())];
    choices.extend(
        app.prs
            .iter()
            .enumerate()
            .map(|(i, p)| (Some(i), p.label.clone())),
    );

    let mut spans = vec![Span::raw(" ")];
    let mut x = area.x + 1;
    app.pr_hits.clear();
    for (scope, label) in choices {
        let text = format!(" {label} ");
        let width = text.chars().count() as u16;
        app.pr_hits
            .push((scope, Rect::new(x, area.y, width, 1).intersection(area)));
        x += width + 1;
        let style = if scope == app.scope {
            Style::default()
                .fg(theme::ON_ACCENT)
                .bg(theme::BRAND)
                .add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            theme::muted()
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
        (Screen::Checks, "3", "Checks", {
            let (failed, total) = app.checks.counts();
            if failed > 0 {
                format!("{failed} failed")
            } else {
                total.to_string()
            }
        }),
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
        Screen::Checks => app.checks.hints(),
    };
    if app.prs.len() > 1 {
        hints.push(("[ ]", "PR"));
    }
    hints.extend([("?", "keys"), ("q", "quit")]);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(ui::key_hints(&hints));

    let status = match app.screen {
        Screen::Files => app.files.status(),
        Screen::Threads => app.threads.status(),
        Screen::Checks => None,
    };
    let note = match status {
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
            ("1 2 3", "files / threads / checks"),
            ("tab  ⇧tab", "next / previous screen"),
            ("[  ]", "all PRs / one PR at a time"),
            ("?", "this list"),
            ("q", "quit (waits for pending saves)"),
        ],
    ),
    (
        "Files",
        &[
            ("j  k", "move"),
            ("h  l  ⏎", "fold, unfold, step out"),
            ("⏎", "on a file: its diff (or LAUGH_OPEN_CMD)"),
            ("J  K  esc", "scroll the diff · close it"),
            ("v", "viewed: this file"),
            ("V", "viewed: everything in the folder"),
            ("H", "hide / show viewed files"),
            ("m", "viewed: generated files (asks first)"),
            ("o", "reading order / tree"),
            ("/", "filter: text, glob or regex"),
            ("m", "with a filter: every match (asks first)"),
            ("g  G", "top / bottom"),
        ],
    ),
    (
        "Threads",
        &[
            ("h  l", "previous / next thread"),
            ("j  k", "scroll"),
            ("space", "scroll the thread or the code"),
            ("p  P", "next / previous person"),
            ("a", "send the thread to your agent"),
            ("r", "hide / show resolved"),
            ("f", "open → resolved → outdated → all"),
        ],
    ),
    (
        "Checks",
        &[
            ("j  k", "move between checks (failures first)"),
            ("J  K", "scroll the failing step's log"),
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

    #[test]
    fn tab_steps_through_the_screens_and_wraps() {
        assert_eq!(Screen::Files.step(1), Screen::Threads);
        assert_eq!(Screen::Checks.step(1), Screen::Files);
        assert_eq!(Screen::Files.step(-1), Screen::Checks);
    }
}
