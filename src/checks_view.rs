use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap};

use crate::checks::{Check, CheckState, LogLine, failed_step_excerpt};
use crate::github;
use crate::{theme, ui};

enum LogState {
    Loading,
    Loaded {
        command: Option<String>,
        skipped: usize,
        lines: Vec<LogLine>,
    },
    Failed(String),
}

/// The CI screen: every check on each PR's head commit, failures first, and
/// for a failed GitHub Actions job the failing step's output.
pub struct ChecksView {
    checks: Vec<Check>,
    /// `(owner, repo)` per opened PR, for fetching job logs.
    pr_repos: Vec<(String, String)>,
    pr_labels: Vec<String>,
    scope: Option<usize>,
    table: TableState,
    detail_scroll: u16,
    logs: HashMap<usize, LogState>,
    log_tx: Sender<(usize, Result<String, String>)>,
    log_rx: Receiver<(usize, Result<String, String>)>,
}

fn icon(state: CheckState) -> Span<'static> {
    match state {
        CheckState::Failed => Span::styled("✗", Style::default().fg(theme::RED)),
        CheckState::Running => Span::styled("●", Style::default().fg(theme::YELLOW)),
        CheckState::Passed => Span::styled("✓", Style::default().fg(theme::GREEN)),
        CheckState::Neutral => Span::styled("○", theme::muted()),
        CheckState::Skipped => Span::styled("–", theme::faint()),
    }
}

impl ChecksView {
    pub fn new(
        mut checks: Vec<Check>,
        pr_repos: Vec<(String, String)>,
        pr_labels: Vec<String>,
    ) -> Self {
        checks.sort_by(|a, b| {
            (a.state, a.pr, &a.source, &a.name).cmp(&(b.state, b.pr, &b.source, &b.name))
        });
        let (log_tx, log_rx) = mpsc::channel();
        let mut view = ChecksView {
            checks,
            pr_repos,
            pr_labels,
            scope: None,
            table: TableState::default(),
            detail_scroll: 0,
            logs: HashMap::new(),
            log_tx,
            log_rx,
        };
        view.table.select(Some(0));
        view.clamp();
        view
    }

    fn visible(&self) -> Vec<usize> {
        (0..self.checks.len())
            .filter(|&i| self.scope.is_none_or(|pr| self.checks[i].pr == pr))
            .collect()
    }

    fn selected(&self) -> Option<usize> {
        self.table
            .selected()
            .and_then(|i| self.visible().get(i).copied())
    }

    fn clamp(&mut self) {
        let len = self.visible().len();
        let sel = match self.table.selected() {
            _ if len == 0 => None,
            Some(i) => Some(i.min(len - 1)),
            None => Some(0),
        };
        self.table.select(sel);
    }

    pub fn set_scope(&mut self, scope: Option<usize>) {
        self.scope = scope;
        self.table.select(Some(0));
        self.clamp();
        self.detail_scroll = 0;
    }

    /// `(failed, total)` within the current scope.
    pub fn counts(&self) -> (usize, usize) {
        let visible = self.visible();
        let failed = visible
            .iter()
            .filter(|&&i| self.checks[i].state == CheckState::Failed)
            .count();
        (failed, visible.len())
    }

    /// Collects fetched logs, and starts fetching the selected failed job's
    /// log if it isn't already loaded. Call once per frame.
    pub fn tick(&mut self) {
        for (i, result) in self.log_rx.try_iter() {
            let state = match result {
                Ok(raw) => {
                    let (command, skipped, lines) = failed_step_excerpt(&raw);
                    LogState::Loaded {
                        command,
                        skipped,
                        lines,
                    }
                }
                Err(e) => LogState::Failed(e),
            };
            self.logs.insert(i, state);
        }
        let Some(i) = self.selected() else { return };
        let check = &self.checks[i];
        let (Some(job), CheckState::Failed) = (check.job_id, check.state) else {
            return;
        };
        if self.logs.contains_key(&i) {
            return;
        }
        self.logs.insert(i, LogState::Loading);
        let (owner, repo) = self.pr_repos[check.pr].clone();
        let tx = self.log_tx.clone();
        thread::spawn(move || {
            let result = github::fetch_job_log(&owner, &repo, job).map_err(|e| format!("{e:#}"));
            let _ = tx.send((i, result));
        });
    }

    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("j k", "check"), ("J K", "scroll the log")]
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        let len = self.visible().len();
        let current = self.table.selected().unwrap_or(0);
        let mut moved = |sel: usize| {
            self.table.select(Some(sel));
            self.detail_scroll = 0;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down if len > 0 => moved((current + 1).min(len - 1)),
            KeyCode::Char('k') | KeyCode::Up => moved(current.saturating_sub(1)),
            KeyCode::Char('g') | KeyCode::Home => moved(0),
            KeyCode::Char('G') | KeyCode::End if len > 0 => moved(len - 1),
            KeyCode::Char('J') | KeyCode::PageDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(10);
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(10);
            }
            _ => {}
        }
    }

    pub fn draw(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let [summary, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
        self.draw_summary(f, summary);
        if self.visible().is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "  No checks reported on the latest commit.",
                    theme::muted(),
                )),
                body,
            );
            return;
        }
        let [list, detail] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(body);
        self.draw_list(f, list);
        self.draw_detail(f, detail);
    }

    fn draw_summary(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let mut spans = vec![Span::raw(" ")];
        for state in [
            CheckState::Failed,
            CheckState::Running,
            CheckState::Passed,
            CheckState::Neutral,
            CheckState::Skipped,
        ] {
            let n = self
                .visible()
                .iter()
                .filter(|&&i| self.checks[i].state == state)
                .count();
            if n == 0 {
                continue;
            }
            let label = match state {
                CheckState::Failed => "failed",
                CheckState::Running => "running",
                CheckState::Passed => "passed",
                CheckState::Neutral => "neutral",
                CheckState::Skipped => "skipped",
            };
            spans.push(icon(state));
            spans.push(Span::styled(format!(" {n} {label}   "), theme::text()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_list(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let multi = self.pr_labels.len() > 1 && self.scope.is_none();
        let rows: Vec<Row> = self
            .visible()
            .into_iter()
            .map(|i| {
                let c = &self.checks[i];
                let name_style = match c.state {
                    CheckState::Skipped | CheckState::Neutral => theme::muted(),
                    _ => theme::text(),
                };
                let mut spans = vec![
                    icon(c.state),
                    Span::raw(" "),
                    Span::styled(c.name.clone(), name_style),
                ];
                if multi {
                    spans.push(Span::styled(
                        format!("  {}", self.pr_labels[c.pr]),
                        Style::default().fg(theme::BRAND),
                    ));
                }
                Row::new(vec![
                    Cell::from(Line::from(spans)),
                    Cell::from(Span::styled(
                        c.source.clone().unwrap_or_default(),
                        theme::faint(),
                    )),
                ])
            })
            .collect();
        let table = Table::new(rows, [Constraint::Min(16), Constraint::Percentage(40)])
            .block(ui::panel("Checks", false))
            .row_highlight_style(theme::selected_row())
            .highlight_symbol(Line::from(Span::styled("▌", theme::accent())))
            .highlight_spacing(HighlightSpacing::Always);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_detail(&self, f: &mut ratatui::Frame<'_>, area: Rect) {
        let Some(i) = self.selected() else { return };
        let c = &self.checks[i];
        let mut lines = vec![Line::from(vec![
            icon(c.state),
            Span::styled(format!(" {}", c.name), theme::bold(theme::text())),
        ])];
        if let Some(source) = &c.source {
            lines.push(Line::from(Span::styled(source.clone(), theme::muted())));
        }
        if let Some(step) = &c.failed_step {
            lines.push(Line::from(vec![
                Span::styled("failed at  ", theme::faint()),
                Span::styled(step.clone(), Style::default().fg(theme::RED)),
            ]));
        }
        if let Some(url) = &c.url {
            lines.push(Line::from(Span::styled(url.clone(), theme::faint())));
        }

        if !c.annotations.is_empty() {
            lines.push(Line::raw(""));
            for a in &c.annotations {
                let color = if a.level == "failure" {
                    theme::RED
                } else {
                    theme::YELLOW
                };
                let place = match a.line {
                    Some(line) => format!("{}:{line}", a.path),
                    None => a.path.clone(),
                };
                lines.push(Line::from(vec![
                    Span::styled("● ", Style::default().fg(color)),
                    Span::styled(place, theme::accent()),
                ]));
                for l in a.message.lines() {
                    lines.push(Line::from(Span::styled(format!("  {l}"), theme::text())));
                }
            }
        }

        if c.state == CheckState::Failed {
            lines.push(Line::raw(""));
            match (c.job_id, self.logs.get(&i)) {
                (None, _) => lines.push(Line::from(Span::styled(
                    "Not a GitHub Actions job — open the link for its log.",
                    theme::muted(),
                ))),
                (Some(_), None | Some(LogState::Loading)) => lines.push(Line::from(Span::styled(
                    "fetching the log…",
                    theme::muted(),
                ))),
                (Some(_), Some(LogState::Failed(e))) => lines.push(Line::from(Span::styled(
                    e.clone(),
                    Style::default().fg(theme::YELLOW),
                ))),
                (
                    Some(_),
                    Some(LogState::Loaded {
                        command,
                        skipped,
                        lines: log,
                    }),
                ) => {
                    if let Some(command) = command {
                        lines.push(Line::from(vec![
                            Span::styled("$ ", theme::faint()),
                            Span::styled(command.clone(), theme::bold(theme::text())),
                        ]));
                    }
                    if *skipped > 0 {
                        lines.push(Line::from(Span::styled(
                            format!("… {skipped} earlier lines"),
                            theme::faint(),
                        )));
                    }
                    for l in log {
                        lines.push(match l {
                            LogLine::Error(t) => Line::from(Span::styled(
                                format!("error: {t}"),
                                theme::bold(Style::default().fg(theme::RED)),
                            )),
                            LogLine::Warning(t) => Line::from(Span::styled(
                                format!("warning: {t}"),
                                Style::default().fg(theme::YELLOW),
                            )),
                            LogLine::Command(t) => {
                                Line::from(Span::styled(format!("$ {t}"), theme::muted()))
                            }
                            LogLine::Output(t) => {
                                Line::from(Span::styled(t.clone(), theme::text()))
                            }
                        });
                    }
                }
            }
        }

        f.render_widget(
            Paragraph::new(lines)
                .block(ui::panel("Detail", true))
                .wrap(Wrap { trim: false })
                .scroll((self.detail_scroll, 0)),
            area,
        );
    }
}
