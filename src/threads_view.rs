use std::cell::Cell;

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::format::{clean_body, snippet};
use crate::handoff;
use crate::model::Thread;
use crate::{theme, ui};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateFilter {
    All,
    Open,
    Resolved,
    Outdated,
}

impl StateFilter {
    fn matches(self, thread: &Thread) -> bool {
        match self {
            StateFilter::All => true,
            StateFilter::Open => !thread.is_resolved,
            StateFilter::Resolved => thread.is_resolved,
            StateFilter::Outdated => thread.is_outdated,
        }
    }

    fn label(self) -> &'static str {
        match self {
            StateFilter::All => "all",
            StateFilter::Open => "open",
            StateFilter::Resolved => "resolved",
            StateFilter::Outdated => "outdated",
        }
    }

    fn cycle(self) -> Self {
        match self {
            StateFilter::All => StateFilter::Open,
            StateFilter::Open => StateFilter::Resolved,
            StateFilter::Resolved => StateFilter::Outdated,
            StateFilter::Outdated => StateFilter::All,
        }
    }
}

const CARD_WIDTH: u16 = 30;
const CARD_HEIGHT: u16 = 5;

/// One tab in the tab bar: either "All" (no starter filter) or a single
/// person/bot who opened threads on this PR.
struct Tab {
    login: Option<String>,
    is_bot: bool,
}

impl Tab {
    fn label(&self) -> &str {
        self.login.as_deref().unwrap_or("All")
    }

    fn matches(&self, thread: &Thread) -> bool {
        match &self.login {
            None => true,
            Some(login) => thread.starter().is_some_and(|c| &c.author == login),
        }
    }
}

/// Builds the tab list from who opened each thread: "All" first, then every
/// distinct starter, humans before bots (alphabetically within each group)
/// since AI review noise is the thing you want to step around, not hunt for.
fn build_tabs(threads: &[Thread]) -> Vec<Tab> {
    let mut seen: Vec<(String, bool)> = Vec::new();
    for t in threads {
        if let Some(starter) = t.starter()
            && !seen.iter().any(|(login, _)| login == &starter.author)
        {
            seen.push((starter.author.clone(), starter.author_is_bot));
        }
    }
    seen.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut tabs = vec![Tab {
        login: None,
        is_bot: false,
    }];
    tabs.extend(seen.into_iter().map(|(login, is_bot)| Tab {
        login: Some(login),
        is_bot,
    }));
    tabs
}

/// The first human-started tab, if there is one — the default view, since
/// bot review threads aren't what you want to see by default.
fn default_tab_index(tabs: &[Tab]) -> usize {
    tabs.iter()
        .position(|t| t.login.is_some() && !t.is_bot)
        .unwrap_or(0)
}

/// Which of the two content panes j/k currently scrolls.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Thread,
    Code,
}

impl Pane {
    fn toggled(self) -> Self {
        match self {
            Pane::Thread => Pane::Code,
            Pane::Code => Pane::Thread,
        }
    }
}

/// The review-threads screen: thread cards grouped by who started them, the
/// selected thread's conversation on the left and its code on the right.
pub struct ThreadsView {
    threads: Vec<Thread>,
    /// `owner/repo#N`-style labels per opened PR; shown on cards and the
    /// code pane only when there is more than one.
    pr_labels: Vec<String>,
    /// `owner/repo#N` per opened PR, for what's handed to an agent.
    pr_refs: Vec<String>,
    /// `None` shows every open PR's threads together; `Some(i)` just the i-th.
    scope: Option<usize>,
    /// The outcome of the last hand-off to an agent, shown in the footer.
    status: Option<String>,
    state_filter: StateFilter,
    tabs: Vec<Tab>,
    selected_tab: usize,
    selected: Option<usize>,
    focus: Pane,
    thread_scroll: u16,
    /// `None` keeps the code pane centred on the commented line.
    code_scroll: Option<u16>,
    /// Where that automatic position currently is, so the first manual
    /// scroll continues from it instead of jumping to the top.
    code_auto_scroll: Cell<u16>,
}

impl ThreadsView {
    pub fn new(threads: Vec<Thread>, pr_labels: Vec<String>, pr_refs: Vec<String>) -> Self {
        let tabs = build_tabs(&threads);
        let selected_tab = default_tab_index(&tabs);
        let mut view = ThreadsView {
            threads,
            pr_labels,
            pr_refs,
            scope: None,
            status: None,
            state_filter: StateFilter::All,
            tabs,
            selected_tab,
            selected: None,
            focus: Pane::Thread,
            thread_scroll: 0,
            code_scroll: None,
            code_auto_scroll: Cell::new(0),
        };
        view.clamp_selection();
        view
    }

    /// Indices matching the state filter, not yet narrowed to the selected
    /// tab. Used both to build per-tab counts and as the base for
    /// `visible_indices`.
    fn base_indices(&self) -> Vec<usize> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| self.state_filter.matches(t))
            .filter(|(_, t)| self.scope.is_none_or(|pr| t.pr == pr))
            .map(|(i, _)| i)
            .collect()
    }

    fn visible_indices(&self) -> Vec<usize> {
        let base = self.base_indices();
        let Some(tab) = self.tabs.get(self.selected_tab) else {
            return base;
        };
        base.into_iter()
            .filter(|&i| tab.matches(&self.threads[i]))
            .collect()
    }

    fn tab_count(&self, tab: &Tab) -> usize {
        self.base_indices()
            .iter()
            .filter(|&&i| tab.matches(&self.threads[i]))
            .count()
    }

    fn selected_thread(&self) -> Option<&Thread> {
        let indices = self.visible_indices();
        let sel = self.selected?;
        indices.get(sel).map(|&i| &self.threads[i])
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.selected = None;
            return;
        }
        let sel = self.selected.unwrap_or(0).min(len - 1);
        self.selected = Some(sel);
    }

    fn switch_tab(&mut self, delta: i32) {
        let len = self.tabs.len() as i32;
        if len == 0 {
            return;
        }
        let current = self.selected_tab as i32;
        self.selected_tab = ((current + delta).rem_euclid(len)) as usize;
        self.selected = Some(0);
        self.clamp_selection();
        self.reset_scroll();
    }

    fn move_card(&mut self, delta: i32) {
        let len = self.visible_indices().len();
        if len == 0 {
            return;
        }
        let current = self.selected.unwrap_or(0) as i32;
        let next = (current + delta).clamp(0, len as i32 - 1);
        self.selected = Some(next as usize);
        self.reset_scroll();
    }

    /// A direct on/off toggle for the single most common filter — separate
    /// from `f`'s full open/resolved/outdated/all cycle, since "hide the
    /// ones I've already resolved" is a one-key gesture people reach for
    /// far more often than the other states.
    fn toggle_hide_resolved(&mut self) {
        self.state_filter = if self.state_filter == StateFilter::Open {
            StateFilter::All
        } else {
            StateFilter::Open
        };
        self.clamp_selection();
        self.reset_scroll();
    }

    fn reset_scroll(&mut self) {
        self.thread_scroll = 0;
        self.code_scroll = None;
    }

    fn scroll_focused(&mut self, delta: i32) {
        match self.focus {
            Pane::Thread => {
                self.thread_scroll = self.thread_scroll.saturating_add_signed(delta as i16);
            }
            Pane::Code => {
                let from = self
                    .code_scroll
                    .unwrap_or_else(|| self.code_auto_scroll.get());
                self.code_scroll = Some(from.saturating_add_signed(delta as i16));
            }
        }
    }
}

impl ThreadsView {
    /// Threads within the current scope, before any other filter.
    pub fn len(&self) -> usize {
        self.threads
            .iter()
            .filter(|t| self.scope.is_none_or(|pr| t.pr == pr))
            .count()
    }

    pub fn set_scope(&mut self, scope: Option<usize>) {
        self.scope = scope;
        self.selected = Some(0);
        self.clamp_selection();
        self.reset_scroll();
    }

    /// The PR a thread belongs to, when telling PRs apart matters.
    fn pr_badge(&self, thread: &Thread) -> Option<&str> {
        (self.pr_labels.len() > 1 && self.scope.is_none())
            .then(|| self.pr_labels[thread.pr].as_str())
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Writes the selected thread up as a prompt and hands it to an agent.
    fn hand_off(&mut self) {
        let Some(thread) = self.selected_thread() else {
            return;
        };
        let pr = self.pr_refs.get(thread.pr).map_or("", String::as_str);
        let prompt = handoff::thread_prompt(pr, thread);
        self.status = Some(match handoff::deliver(&prompt) {
            Ok(delivered) => delivered.describe(),
            Err(e) => format!("couldn't hand the thread off: {e:#}"),
        });
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        self.status = None;
        match code {
            KeyCode::Char('a') => self.hand_off(),
            KeyCode::Char('l') | KeyCode::Right => self.move_card(1),
            KeyCode::Char('h') | KeyCode::Left => self.move_card(-1),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_focused(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_focused(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = Some(0);
                self.clamp_selection();
                self.reset_scroll();
            }
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.visible_indices().len();
                self.selected = len.checked_sub(1);
                self.reset_scroll();
            }
            KeyCode::Char('p') => self.switch_tab(1),
            KeyCode::Char('P') => self.switch_tab(-1),
            KeyCode::Char(' ') | KeyCode::Enter => self.focus = self.focus.toggled(),
            KeyCode::Char('f') => {
                self.state_filter = self.state_filter.cycle();
                self.clamp_selection();
                self.reset_scroll();
            }
            KeyCode::Char('r') => self.toggle_hide_resolved(),
            KeyCode::Char('J') | KeyCode::PageDown => self.scroll_focused(10),
            KeyCode::Char('K') | KeyCode::PageUp => self.scroll_focused(-10),
            _ => {}
        }
    }

    /// Key hints for the footer.
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let resolved = if self.state_filter == StateFilter::Open {
            ("r", "show resolved")
        } else {
            ("r", "hide resolved")
        };
        vec![
            ("←→", "thread"),
            ("↑↓", "scroll"),
            ("space", "code ⇄ thread"),
            ("p P", "person"),
            ("a", "to agent"),
            resolved,
        ]
    }

    pub fn draw(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let [people, cards, body] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(CARD_HEIGHT),
            Constraint::Min(0),
        ])
        .areas(area);

        draw_people(f, people, self);
        draw_cards(f, cards, self);

        let [thread, code] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(body);
        draw_thread_content(f, thread, self);
        draw_code(f, code, self);
    }
}

fn state_badge(thread: &Thread) -> Vec<Span<'static>> {
    let mut spans = if thread.is_resolved {
        vec![Span::styled(
            "✔ resolved",
            Style::default().fg(theme::GREEN),
        )]
    } else {
        vec![Span::styled("● open", Style::default().fg(theme::YELLOW))]
    };
    if thread.is_outdated {
        spans.push(Span::styled("  outdated", theme::faint()));
    }
    spans
}

fn location(thread: &Thread) -> String {
    match thread.display_line() {
        Some(line) => format!("{}:{}", thread.path.as_deref().unwrap_or("?"), line),
        None => thread.path.clone().unwrap_or_else(|| "?".to_string()),
    }
}

/// Who started the threads, as pills: humans first, then bots after a rule.
fn draw_people(f: &mut ratatui::Frame<'_>, area: Rect, app: &ThreadsView) {
    let mut spans = vec![Span::raw(" ")];
    let mut seen_bot = false;
    for (i, tab) in app.tabs.iter().enumerate() {
        if tab.is_bot && !seen_bot {
            seen_bot = true;
            spans.push(Span::styled(" │ ", theme::faint()));
        } else if i > 0 {
            spans.push(Span::raw(" "));
        }
        let count = app.tab_count(tab);
        let style = if i == app.selected_tab {
            theme::pill_on()
        } else if tab.is_bot {
            theme::muted()
        } else {
            theme::text()
        };
        spans.push(Span::styled(format!(" {} ", tab.label()), style));
        let count_style = if i == app.selected_tab {
            theme::bold(theme::accent())
        } else {
            theme::faint()
        };
        spans.push(Span::styled(format!("{count} "), count_style));
    }
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(18)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), left);
    if app.state_filter != StateFilter::All {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("showing ", theme::faint()),
                Span::styled(app.state_filter.label(), theme::accent()),
                Span::raw(" "),
            ]))
            .right_aligned(),
            right,
        );
    }
}

/// The visible threads as a horizontal strip of cards, scrolled so the
/// selected card always stays in view.
fn draw_cards(f: &mut ratatui::Frame<'_>, area: Rect, app: &ThreadsView) {
    let indices = app.visible_indices();
    if indices.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Nothing here — tab to someone else, or r to show resolved threads.",
                theme::muted(),
            ))),
            area,
        );
        return;
    }

    let visible_count = ((area.width / CARD_WIDTH).max(1) as usize).min(indices.len());
    let selected = app.selected.unwrap_or(0);
    let start = if indices.len() <= visible_count {
        0
    } else {
        selected
            .saturating_sub(visible_count / 2)
            .min(indices.len() - visible_count)
    };
    let window = &indices[start..start + visible_count];

    let mut constraints: Vec<Constraint> = window
        .iter()
        .map(|_| Constraint::Length(CARD_WIDTH))
        .collect();
    constraints.push(Constraint::Min(0));
    let chunks = Layout::horizontal(constraints).split(area);

    for (slot, &thread_idx) in window.iter().enumerate() {
        let t = &app.threads[thread_idx];
        let is_selected = start + slot == selected;

        let filename = t
            .path
            .as_deref()
            .and_then(|p| p.rsplit('/').next())
            .unwrap_or("?");
        let snippet_text = t
            .starter()
            .map(|c| snippet(&c.body, (CARD_WIDTH as usize).saturating_sub(4)))
            .unwrap_or_default();
        let mut status = state_badge(t);
        let replies = t.comments.len().saturating_sub(1);
        if replies > 0 {
            status.push(Span::styled(format!("  ↩{replies}"), theme::faint()));
        }

        let name_style = if is_selected {
            theme::bold(theme::text())
        } else {
            theme::text()
        };
        let lines = vec![
            Line::from(status),
            Line::from(Span::styled(filename.to_string(), name_style)),
            Line::from(Span::styled(snippet_text, theme::muted())),
        ];
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if is_selected {
                theme::accent()
            } else {
                theme::faint()
            });
        // Which PR, on the card's top edge, so it never crowds the content.
        if let Some(badge) = app.pr_badge(t) {
            block = block.title(Span::styled(
                format!(" {badge} "),
                Style::default().fg(theme::BRAND),
            ));
        }
        // Tint only inside the border: a background under the border cells
        // reads as a smudge around the rounded corners.
        let inner = block.inner(chunks[slot]);
        f.render_widget(block, chunks[slot]);
        let mut card = Paragraph::new(lines);
        if is_selected {
            card = card.style(theme::selected_row());
        }
        f.render_widget(card, inner);
    }

    if indices.len() > visible_count {
        let more = Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{}/{}", selected + 1, indices.len()),
                theme::muted(),
            ),
            Span::raw(" "),
        ]))
        .right_aligned();
        let label = Rect {
            y: area.y + area.height.saturating_sub(1),
            height: 1,
            ..chunks[visible_count]
        };
        f.render_widget(more, label);
    }
}

/// Left pane: the thread's conversation (who said what), no code.
fn draw_thread_content(f: &mut ratatui::Frame<'_>, area: Rect, app: &ThreadsView) {
    let focused = app.focus == Pane::Thread;
    let Some(thread) = app.selected_thread() else {
        f.render_widget(ui::panel("Thread", focused), area);
        return;
    };
    let position = format!(
        "Thread {}/{}",
        app.selected.unwrap_or(0) + 1,
        app.visible_indices().len()
    );

    let mut lines: Vec<Line> = vec![Line::from(state_badge(thread)), Line::raw("")];
    for comment in &thread.comments {
        let who = if comment.author_is_bot {
            theme::muted()
        } else {
            theme::accent()
        };
        let mut header = vec![
            Span::styled("● ", who),
            Span::styled(comment.author.clone(), theme::bold(who)),
        ];
        if comment.author_is_bot {
            header.push(Span::styled(" bot", theme::faint()));
        }
        if let Some(ts) = &comment.created_at {
            header.push(Span::styled(
                format!("  {}", ui::relative_time(ts)),
                theme::faint(),
            ));
        }
        lines.push(Line::from(header));
        lines.extend(ui::markdown(&clean_body(&comment.body)));
        lines.push(Line::raw(""));
    }

    let paragraph = Paragraph::new(lines)
        .block(ui::panel(&position, focused))
        .wrap(Wrap { trim: false })
        .scroll((app.thread_scroll, 0));
    f.render_widget(paragraph, area);
}

/// The hunk with a line-number gutter, added / removed lines tinted, and the
/// line the comment is on highlighted. Lines are padded to `width` so the
/// tints run the full width of the pane. Returns the lines and the index of
/// the highlighted one.
fn hunk_lines(hunk: &str, target: Option<i64>, width: usize) -> (Vec<Line<'static>>, usize) {
    let (mut lines, numbered) = ui::diff_lines(hunk, width);
    // GitHub's diffHunk ends on the commented line, so fall back to the last.
    let target_index = target
        .and_then(|t| numbered.iter().rposition(|n| *n == Some(t)))
        .unwrap_or(lines.len().saturating_sub(1));
    if let Some(line) = lines.get_mut(target_index) {
        *line = line.clone().style(Style::default().bg(theme::TARGET_BG));
        if let Some(gutter) = line.spans.first_mut() {
            gutter.style = theme::bold(Style::default().fg(theme::YELLOW));
        }
    }
    (lines, target_index)
}

/// Right pane: the code the thread was written against, scrolled so the
/// commented line is in view until you scroll it yourself.
fn draw_code(f: &mut ratatui::Frame<'_>, area: Rect, app: &ThreadsView) {
    let focused = app.focus == Pane::Code;
    let Some(thread) = app.selected_thread() else {
        f.render_widget(ui::panel("Code", focused), area);
        return;
    };
    let path = thread.path.as_deref().unwrap_or("?");
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let title = match thread.display_line() {
        Some(line) => format!("{file_name}:{line}"),
        None => file_name.to_string(),
    };
    let block = ui::panel(&title, focused);
    let inner = block.inner(area);
    let [path_area, code_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    f.render_widget(block, area);
    let mut path_line = Vec::new();
    if app.pr_labels.len() > 1 {
        path_line.push(Span::styled(
            format!("{}  ", app.pr_labels[thread.pr]),
            Style::default().fg(theme::BRAND),
        ));
    }
    path_line.push(Span::styled(location(thread), theme::faint()));
    f.render_widget(Paragraph::new(Line::from(path_line)), path_area);

    let hunk = thread
        .starter()
        .and_then(|c| c.diff_hunk.as_deref())
        .filter(|h| !h.is_empty());
    let Some(hunk) = hunk else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "No diff context for this thread.",
                theme::muted(),
            )),
            code_area,
        );
        return;
    };

    let target = thread.original_line.or(thread.line);
    let (lines, target_index) = hunk_lines(hunk, target, code_area.width as usize);
    // Commented line about a third of the way down, but never scrolled past
    // the end of the hunk (it usually *is* the end).
    let last_page = (lines.len() as u16).saturating_sub(code_area.height);
    let auto = (target_index as u16)
        .saturating_sub(code_area.height / 3)
        .min(last_page);
    app.code_auto_scroll.set(auto);
    let scroll = app.code_scroll.unwrap_or(auto);
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), code_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_header_gives_old_and_new_starts() {
        assert_eq!(
            ui::hunk_start("@@ -769,8 +800,16 @@ export"),
            Some((769, 800))
        );
        assert_eq!(ui::hunk_start("@@ -0,0 +1 @@"), Some((0, 1)));
    }

    #[test]
    fn gutter_numbers_follow_new_lines_and_skip_removed_ones() {
        let hunk = "@@ -10,3 +20,3 @@\n ctx\n-gone\n+added\n tail";
        let (lines, target) = hunk_lines(hunk, Some(22), 40);
        let gutters: Vec<String> = lines
            .iter()
            .map(|l| l.spans[0].content.trim().to_string())
            .collect();
        assert_eq!(gutters[1..], ["20", "", "21", "22"]);
        assert_eq!(target, 4);
    }

    #[test]
    fn missing_target_falls_back_to_the_last_line() {
        let (_, target) = hunk_lines("@@ -1,2 +1,2 @@\n a\n b", None, 40);
        assert_eq!(target, 2);
    }
}
