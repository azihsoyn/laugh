use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState};

use crate::model::{PrFile, ViewedState};
use crate::viewed_sync::{Ledger, Outcome, Worker};
use crate::{theme, ui};

const ROOT: usize = 0;

/// A directory in the changed-file tree. Chains of directories with nothing
/// but a single subdirectory are folded into one node (`a/b/c/`), as GitHub's
/// own tree does — such a chain has exactly one set of files underneath, so
/// splitting it into rows would only add rows that toggle the same files.
#[derive(Debug)]
struct DirNode {
    label: String,
    parent: Option<usize>,
    subdirs: Vec<usize>,
    /// Files directly in this directory.
    files: Vec<usize>,
    /// Every file anywhere underneath — what a directory toggle acts on.
    all_files: Vec<usize>,
    /// The top of one pull request's files, when several PRs are open.
    pr_root: bool,
}

#[derive(Default)]
struct RawDir {
    subdirs: BTreeMap<String, RawDir>,
    files: Vec<usize>,
}

fn raw_tree<'a>(files: impl Iterator<Item = (usize, &'a PrFile)>) -> RawDir {
    let mut root = RawDir::default();
    for (i, f) in files {
        let mut node = &mut root;
        let mut parts: Vec<&str> = f.path.split('/').collect();
        parts.pop();
        for part in parts {
            node = node.subdirs.entry(part.to_string()).or_default();
        }
        node.files.push(i);
    }
    root
}

fn dir_node(label: String, parent: Option<usize>, pr_root: bool) -> DirNode {
    DirNode {
        label,
        parent,
        subdirs: Vec::new(),
        files: Vec::new(),
        all_files: Vec::new(),
        pr_root,
    }
}

/// Builds the tree and returns it with the node each pull request starts at.
/// With one PR that is the root itself; with several, each PR gets its own
/// top-level node so they sit side by side in one tree.
fn build_tree(files: &[PrFile], pr_labels: &[String]) -> (Vec<DirNode>, Vec<usize>) {
    let mut dirs = vec![dir_node(String::new(), None, false)];
    if pr_labels.len() <= 1 {
        fill(&mut dirs, ROOT, raw_tree(files.iter().enumerate()));
        return (dirs, vec![ROOT]);
    }
    let mut roots = Vec::new();
    let mut all = Vec::new();
    for (pr, label) in pr_labels.iter().enumerate() {
        let id = dirs.len();
        dirs.push(dir_node(label.clone(), Some(ROOT), true));
        dirs[ROOT].subdirs.push(id);
        let raw = raw_tree(files.iter().enumerate().filter(|(_, f)| f.pr == pr));
        all.extend(fill(&mut dirs, id, raw));
        roots.push(id);
    }
    dirs[ROOT].all_files = all;
    (dirs, roots)
}

/// Moves `raw`'s contents under `dirs[id]`, creating child nodes, and
/// returns every file now under `id`.
fn fill(dirs: &mut Vec<DirNode>, id: usize, raw: RawDir) -> Vec<usize> {
    let mut all = Vec::new();
    for (name, mut child) in raw.subdirs {
        let mut label = format!("{name}/");
        while child.files.is_empty() && child.subdirs.len() == 1 {
            let (next, grandchild) = child.subdirs.into_iter().next().expect("len is 1");
            label.push_str(&next);
            label.push('/');
            child = grandchild;
        }
        let child_id = dirs.len();
        dirs.push(dir_node(label, Some(id), false));
        dirs[id].subdirs.push(child_id);
        all.extend(fill(dirs, child_id, child));
    }
    all.extend(&raw.files);
    dirs[id].files = raw.files;
    dirs[id].all_files = all.clone();
    all
}

/// What a toggle on `indices` should do: if every one is already viewed,
/// unmark them all; otherwise mark the ones that aren't (DISMISSED included —
/// re-ticking a file that changed since you viewed it is the point).
/// The same rule covers a single file and a directory at any depth.
fn toggle_plan(files: &[PrFile], indices: &[usize]) -> (Vec<usize>, bool) {
    let all_viewed = indices
        .iter()
        .all(|&i| files[i].viewed == ViewedState::Viewed);
    if all_viewed {
        (indices.to_vec(), false)
    } else {
        let pending = indices
            .iter()
            .copied()
            .filter(|&i| files[i].viewed != ViewedState::Viewed)
            .collect();
        (pending, true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Dir(usize),
    File { file: usize, dir: usize },
}

/// The changed-files screen: a directory tree with each file's Viewed state.
pub struct FilesView {
    files: Vec<PrFile>,
    dirs: Vec<DirNode>,
    pr_roots: Vec<usize>,
    /// `None` shows every open PR together; `Some(i)` just the i-th.
    scope: Option<usize>,
    collapsed: HashSet<usize>,
    hide_viewed: bool,
    table: TableState,
    status: Option<String>,
    /// Generated files waiting on a yes / no before they're marked viewed.
    confirm: Option<Vec<usize>>,
    ledger: Ledger,
    /// `None` only in tests, where nothing should reach GitHub.
    worker: Option<Worker>,
}

impl FilesView {
    /// The node the tree is drawn from for the current scope.
    fn view_root(&self) -> usize {
        self.scope.map_or(ROOT, |pr| self.pr_roots[pr])
    }

    pub fn set_scope(&mut self, scope: Option<usize>) {
        self.scope = scope;
        self.table.select(Some(0));
        self.clamp();
    }

    fn file_visible(&self, file: usize) -> bool {
        !(self.hide_viewed && self.files[file].viewed == ViewedState::Viewed)
    }

    fn dir_visible(&self, dir: usize) -> bool {
        self.dirs[dir]
            .all_files
            .iter()
            .any(|&f| self.file_visible(f))
    }

    fn rows(&self) -> Vec<RowRef> {
        let mut rows = Vec::new();
        self.push_contents(self.view_root(), &mut rows);
        rows
    }

    fn push_contents(&self, dir: usize, rows: &mut Vec<RowRef>) {
        for &sub in &self.dirs[dir].subdirs {
            if !self.dir_visible(sub) {
                continue;
            }
            rows.push(RowRef::Dir(sub));
            if !self.collapsed.contains(&sub) {
                self.push_contents(sub, rows);
            }
        }
        for &file in &self.dirs[dir].files {
            if self.file_visible(file) {
                rows.push(RowRef::File { file, dir });
            }
        }
    }

    /// The same rows as `rows`, each with the tree guide (`├─ `, `│  `…)
    /// that draws its place in the hierarchy.
    fn rows_with_guides(&self) -> Vec<(RowRef, String)> {
        let mut rows = Vec::new();
        self.push_guided(self.view_root(), "", true, &mut rows);
        rows
    }

    fn push_guided(&self, dir: usize, prefix: &str, top: bool, rows: &mut Vec<(RowRef, String)>) {
        let node = &self.dirs[dir];
        let children: Vec<RowRef> = node
            .subdirs
            .iter()
            .filter(|&&d| self.dir_visible(d))
            .map(|&d| RowRef::Dir(d))
            .chain(
                node.files
                    .iter()
                    .filter(|&&f| self.file_visible(f))
                    .map(|&file| RowRef::File { file, dir }),
            )
            .collect();
        let count = children.len();
        for (i, child) in children.into_iter().enumerate() {
            let last = i + 1 == count;
            let (branch, carry) = match (top, last) {
                (true, _) => ("", ""),
                (false, true) => ("└─ ", "   "),
                (false, false) => ("├─ ", "│  "),
            };
            rows.push((child, format!("{prefix}{branch}")));
            if let RowRef::Dir(d) = child
                && !self.collapsed.contains(&d)
            {
                self.push_guided(d, &format!("{prefix}{carry}"), false, rows);
            }
        }
    }

    fn current(&self) -> Option<RowRef> {
        let rows = self.rows();
        self.table.selected().and_then(|i| rows.get(i).copied())
    }

    fn clamp(&mut self) {
        let len = self.rows().len();
        let sel = match self.table.selected() {
            _ if len == 0 => None,
            Some(i) => Some(i.min(len - 1)),
            None => Some(0),
        };
        self.table.select(sel);
    }

    fn move_by(&mut self, delta: i64) {
        let len = self.rows().len() as i64;
        if len == 0 {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as i64;
        self.table
            .select(Some((cur + delta).clamp(0, len - 1) as usize));
    }

    fn select_dir(&mut self, dir: usize) {
        if dir == self.view_root() {
            return;
        }
        if let Some(i) = self.rows().iter().position(|r| *r == RowRef::Dir(dir)) {
            self.table.select(Some(i));
        }
    }

    /// Left: fold an open directory; otherwise step out to the parent.
    fn go_left(&mut self, row: RowRef) {
        match row {
            RowRef::Dir(d) if !self.collapsed.contains(&d) => {
                self.collapsed.insert(d);
            }
            RowRef::Dir(d) => {
                if let Some(parent) = self.dirs[d].parent {
                    self.select_dir(parent);
                }
            }
            RowRef::File { dir, .. } => self.select_dir(dir),
        }
    }

    fn viewed_count(&self, indices: &[usize]) -> usize {
        indices
            .iter()
            .filter(|&&i| self.files[i].viewed == ViewedState::Viewed)
            .count()
    }

    /// Changes the screen immediately and queues the write to GitHub.
    fn apply_toggle(&mut self, indices: &[usize]) {
        let (targets, viewed) = toggle_plan(&self.files, indices);
        if targets.is_empty() {
            return;
        }
        let jobs = self.ledger.apply(&mut self.files, &targets, viewed);
        if let Some(worker) = &self.worker {
            for job in jobs {
                worker.send(job);
            }
        }
        self.clamp();
    }

    fn settle(&mut self, outcome: Outcome) {
        let reverted = self
            .ledger
            .settle(&mut self.files, outcome.id, outcome.error.is_none());
        if let Some(error) = outcome.error {
            self.status = Some(format!(
                "couldn't save to GitHub, reverted {reverted} file(s): {error}"
            ));
            self.clamp();
        }
    }

    fn drain_outcomes(&mut self) {
        let outcomes: Vec<Outcome> = match &self.worker {
            Some(worker) => worker.outcomes.try_iter().collect(),
            None => return,
        };
        for outcome in outcomes {
            self.settle(outcome);
        }
    }

    /// Viewed and total files within the current scope.
    pub fn viewed_counts(&self) -> (usize, usize) {
        let in_scope = &self.dirs[self.view_root()].all_files;
        (self.viewed_count(in_scope), in_scope.len())
    }

    pub fn pending(&self) -> usize {
        self.ledger.pending()
    }

    pub fn show_saving(&mut self) {
        self.status = Some(format!(
            "saving {} pending change(s) to GitHub…",
            self.ledger.pending()
        ));
    }

    /// Lets every queued write reach GitHub before exiting, so quitting right
    /// after a toggle doesn't silently drop it.
    pub fn finish(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let failures: Vec<String> = worker
            .finish()
            .into_iter()
            .filter_map(|o| o.error)
            .collect();
        if !failures.is_empty() {
            bail!(
                "{} Viewed change(s) didn't reach GitHub: {}",
                failures.len(),
                failures.join("; ")
            );
        }
        Ok(())
    }

    /// `pr_ids` / `pr_labels` are per opened pull request; each file's `pr`
    /// indexes into them.
    pub fn new(pr_ids: Vec<String>, pr_labels: &[String], files: Vec<PrFile>) -> Self {
        let (dirs, pr_roots) = build_tree(&files, pr_labels);
        let mut view = FilesView {
            dirs,
            pr_roots,
            scope: None,
            ledger: Ledger::new(&files),
            files,
            collapsed: HashSet::new(),
            hide_viewed: false,
            table: TableState::default(),
            status: None,
            confirm: None,
            worker: Some(Worker::spawn(pr_ids)),
        };
        view.clamp();
        view
    }

    /// Picks up finished background writes; call once per frame.
    pub fn tick(&mut self) {
        self.drain_outcomes();
    }

    /// Generated files in scope that aren't viewed yet.
    fn unviewed_generated(&self) -> Vec<usize> {
        self.dirs[self.view_root()]
            .all_files
            .iter()
            .copied()
            .filter(|&f| {
                self.files[f].generated.is_some() && self.files[f].viewed != ViewedState::Viewed
            })
            .collect()
    }

    /// True while the confirmation dialog is open and should get every key.
    pub fn is_modal(&self) -> bool {
        self.confirm.is_some()
    }

    fn ask_to_mark_generated(&mut self) {
        let targets = self.unviewed_generated();
        if targets.is_empty() {
            self.status = Some("no unviewed generated files here".to_string());
        } else {
            self.confirm = Some(targets);
        }
    }

    fn answer_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let targets = self.confirm.take().unwrap_or_default();
                let count = targets.len();
                let jobs = self.ledger.apply(&mut self.files, &targets, true);
                if let Some(worker) = &self.worker {
                    for job in jobs {
                        worker.send(job);
                    }
                }
                self.status = Some(format!("{count} generated file(s) marked viewed"));
                self.clamp();
            }
            KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => self.confirm = None,
            _ => {}
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        self.status = None;
        if self.is_modal() {
            self.answer_confirm(code);
            return;
        }
        if code == KeyCode::Char('m') {
            self.ask_to_mark_generated();
            return;
        }
        let Some(current) = self.current() else {
            if code == KeyCode::Char('H') {
                self.hide_viewed = !self.hide_viewed;
                self.clamp();
            }
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Char('g') | KeyCode::Home => self.table.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => {
                let len = self.rows().len();
                self.table.select(len.checked_sub(1));
            }
            // Viewed changes are writes to GitHub, so they sit on deliberate
            // letter keys only — never on space, which is easy to hit by accident.
            KeyCode::Char('v') => match current {
                RowRef::File { file, .. } => self.apply_toggle(&[file]),
                RowRef::Dir(_) => {
                    self.status =
                        Some("v toggles a single file — use V for a whole directory".to_string());
                }
            },
            KeyCode::Char('V') => {
                // On a file, V means "the directory this file sits in".
                // Files at the repository root have no directory row, so
                // they fall back to just themselves rather than the whole PR.
                let targets = match current {
                    RowRef::Dir(d) => self.dirs[d].all_files.clone(),
                    RowRef::File { dir, .. } if dir != self.view_root() => {
                        self.dirs[dir].all_files.clone()
                    }
                    RowRef::File { file, .. } => vec![file],
                };
                self.apply_toggle(&targets);
            }
            KeyCode::Enter => match current {
                RowRef::Dir(d) => {
                    if !self.collapsed.remove(&d) {
                        self.collapsed.insert(d);
                    }
                }
                RowRef::File { .. } => {
                    self.status = Some("opening a file isn't wired up yet".to_string());
                }
            },
            KeyCode::Char('l') | KeyCode::Right => {
                if let RowRef::Dir(d) = current {
                    self.collapsed.remove(&d);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.go_left(current),
            KeyCode::Char('H') => {
                self.hide_viewed = !self.hide_viewed;
                self.clamp();
            }
            _ => {}
        }
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Key hints for the footer, for whatever the cursor is on.
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let hide = if self.hide_viewed {
            ("H", "show viewed")
        } else {
            ("H", "hide viewed")
        };
        if self.is_modal() {
            return vec![("y", "mark them viewed"), ("n", "cancel")];
        }
        let mut hints = match self.current() {
            Some(RowRef::Dir(_)) => vec![("V", "viewed: everything inside"), ("⏎", "fold"), hide],
            Some(RowRef::File { .. }) => vec![("v", "viewed"), ("V", "whole folder"), hide],
            None => vec![hide],
        };
        if !self.unviewed_generated().is_empty() {
            hints.push(("m", "viewed: generated"));
        }
        hints
    }

    pub fn draw(&mut self, f: &mut ratatui::Frame<'_>, area: Rect) {
        draw(f, area, self);
    }
}

fn viewed_icon(state: ViewedState) -> Span<'static> {
    match state {
        ViewedState::Viewed => Span::styled("✓", Style::default().fg(theme::GREEN)),
        ViewedState::Dismissed => Span::styled("⟳", Style::default().fg(theme::YELLOW)),
        ViewedState::Unviewed => Span::styled("○", theme::faint()),
    }
}

/// `+N` or `-N` right-aligned in its own column, blank for zero, so the
/// added and removed counts each line up down the tree.
fn count_cell(n: u64, sign: char, color: ratatui::style::Color) -> Cell<'static> {
    if n == 0 {
        return Cell::from("");
    }
    Cell::from(
        Line::from(Span::styled(
            format!("{sign}{n}"),
            Style::default().fg(color),
        ))
        .right_aligned(),
    )
}

/// "Mark these N generated files viewed?" — listing what and why, since it
/// writes to GitHub for files the reviewer hasn't opened.
fn draw_confirm(f: &mut ratatui::Frame<'_>, area: Rect, app: &FilesView, targets: &[usize]) {
    const SHOWN: usize = 12;
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Mark ", theme::text()),
            Span::styled(format!("{}", targets.len()), theme::bold(theme::accent())),
            Span::styled(" generated file(s) as viewed?", theme::text()),
        ]),
        Line::raw(""),
    ];
    for &i in targets.iter().take(SHOWN) {
        let file = &app.files[i];
        lines.push(Line::from(vec![
            Span::raw("  "),
            viewed_icon(file.viewed),
            Span::raw(" "),
            Span::styled(file.path.clone(), theme::text()),
            Span::styled(
                format!("  {}", file.generated.unwrap_or("")),
                theme::faint(),
            ),
        ]));
    }
    if targets.len() > SHOWN {
        lines.push(Line::from(Span::styled(
            format!("  …and {} more", targets.len() - SHOWN),
            theme::muted(),
        )));
    }
    lines.push(Line::raw(""));
    let mut keys = vec![Span::raw("  ")];
    keys.extend(ui::key_hints(&[("y", "mark them viewed"), ("n", "cancel")]));
    lines.push(Line::from(keys));

    let width = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(40)
        .saturating_add(6)
        .clamp(44, area.width.saturating_sub(4));
    let popup = ui::centered(area, width, lines.len() as u16 + 2);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(ui::panel("Generated files", true)),
        popup,
    );
}

fn draw(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut FilesView) {
    let [summary_area, list_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);

    let (viewed, total) = app.viewed_counts();
    let dismissed = app.dirs[app.view_root()]
        .all_files
        .iter()
        .filter(|&&f| app.files[f].viewed == ViewedState::Dismissed)
        .count();

    let mut summary = vec![Span::styled(" Viewed  ", theme::muted())];
    summary.extend(ui::gauge(viewed, total, 28));
    summary.push(Span::styled(
        format!("  {viewed}/{total}"),
        theme::bold(theme::text()),
    ));
    if dismissed > 0 {
        summary.push(Span::styled("   ⟳ ", Style::default().fg(theme::YELLOW)));
        summary.push(Span::styled(
            format!("{dismissed} changed since viewed"),
            Style::default().fg(theme::YELLOW),
        ));
    }
    if app.hide_viewed {
        summary.push(Span::raw("   "));
        summary.push(Span::styled(" hiding viewed ", theme::pill_on()));
    }
    f.render_widget(Paragraph::new(Line::from(summary)), summary_area);

    // Pad both sides of `done/total` to the widest total any directory row
    // shows (the root itself is never a row), so the slashes line up.
    let digits = app
        .dirs
        .iter()
        .skip(1)
        .map(|d| d.all_files.len().to_string().len())
        .max()
        .unwrap_or(1);
    let rows: Vec<Row> = app
        .rows_with_guides()
        .into_iter()
        .map(|(r, guide)| {
            let guide = Span::styled(guide, theme::faint());
            match r {
                RowRef::Dir(d) => {
                    let node = &app.dirs[d];
                    let done = app.viewed_count(&node.all_files);
                    let total = node.all_files.len();
                    let fold = if app.collapsed.contains(&d) {
                        "▸ "
                    } else {
                        "▾ "
                    };
                    let (adds, dels) = node.all_files.iter().fold((0, 0), |(a, d), &i| {
                        (a + app.files[i].additions, d + app.files[i].deletions)
                    });
                    let mut name = vec![guide, Span::styled(fold, theme::muted())];
                    if node.pr_root {
                        name.push(Span::styled("▣ ", Style::default().fg(theme::BRAND)));
                        name.push(Span::styled(node.label.clone(), theme::bold(theme::text())));
                    } else {
                        name.push(Span::styled(
                            node.label.clone(),
                            theme::bold(theme::accent()),
                        ));
                    }
                    if done == total {
                        name.push(Span::styled(" ✓", Style::default().fg(theme::GREEN)));
                    }
                    Row::new(vec![
                        Cell::from(Line::from(name)),
                        Cell::from(Line::from(ui::gauge(done, total, 8))),
                        Cell::from(Span::styled(
                            format!("{done:>digits$}/{total:>digits$}"),
                            theme::muted(),
                        )),
                        count_cell(adds, '+', theme::GREEN),
                        count_cell(dels, '-', theme::RED),
                    ])
                }
                RowRef::File { file, .. } => {
                    let pf = &app.files[file];
                    let name_style = match pf.viewed {
                        ViewedState::Viewed => theme::muted(),
                        _ => theme::text(),
                    };
                    Row::new(vec![
                        Cell::from(Line::from(vec![
                            guide,
                            viewed_icon(pf.viewed),
                            Span::raw(" "),
                            Span::styled(pf.file_name().to_string(), name_style),
                            Span::styled(
                                pf.generated.map_or(String::new(), |why| format!("  {why}")),
                                theme::faint(),
                            ),
                        ])),
                        Cell::from(""),
                        Cell::from(""),
                        count_cell(pf.additions, '+', theme::GREEN),
                        count_cell(pf.deletions, '-', theme::RED),
                    ])
                }
            }
        })
        .collect();

    // Wide enough for the biggest count on screen: every directory total
    // is at most the whole tree's, so size the columns from that.
    let (all_adds, all_dels) = app
        .files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let width = |n: u64| format!("+{n}").len() as u16;
    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(2 * digits as u16 + 1),
            Constraint::Length(width(all_adds)),
            Constraint::Length(width(all_dels)),
        ],
    )
    .column_spacing(2)
    .block(ui::panel(&format!("{total} files changed"), false))
    .row_highlight_style(theme::selected_row())
    .highlight_symbol(Line::from(Span::styled("▌", theme::accent())))
    .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, list_area, &mut app.table);

    if let Some(targets) = &app.confirm {
        draw_confirm(f, area, app, targets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, viewed: ViewedState) -> PrFile {
        PrFile {
            pr: 0,
            path: path.to_string(),
            additions: 1,
            deletions: 0,
            viewed,
            generated: None,
        }
    }

    fn files(paths: &[&str]) -> Vec<PrFile> {
        paths
            .iter()
            .map(|p| file(p, ViewedState::Unviewed))
            .collect()
    }

    fn label_of(dirs: &[DirNode], id: usize) -> &str {
        &dirs[id].label
    }

    #[test]
    fn single_child_chains_fold_into_one_node() {
        let (dirs, _) = build_tree(
            &files(&[
                "packages/core/src/index.ts",
                "packages/core/src/index.spec.ts",
                ".changeset/x.md",
            ]),
            &[],
        );
        let top: Vec<&str> = dirs[ROOT]
            .subdirs
            .iter()
            .map(|&d| label_of(&dirs, d))
            .collect();
        assert_eq!(top, [".changeset/", "packages/core/src/"]);
    }

    #[test]
    fn branching_directory_keeps_its_children_and_owns_every_file_below() {
        // a/ branches into b/ and c/ and has a file of its own.
        let (dirs, _) = build_tree(&files(&["a/b/x.ts", "a/c/d/y.ts", "a/z.ts"]), &[]);
        let a = dirs[ROOT].subdirs[0];
        assert_eq!(label_of(&dirs, a), "a/");
        let children: Vec<&str> = dirs[a]
            .subdirs
            .iter()
            .map(|&d| label_of(&dirs, d))
            .collect();
        assert_eq!(children, ["b/", "c/d/"]);
        assert_eq!(dirs[a].files, [2]);
        let mut under_a = dirs[a].all_files.clone();
        under_a.sort();
        assert_eq!(under_a, [0, 1, 2]);
    }

    #[test]
    fn partial_directory_marks_only_the_unviewed_and_dismissed() {
        let files = vec![
            file("d/a", ViewedState::Viewed),
            file("d/b", ViewedState::Unviewed),
            file("d/c", ViewedState::Dismissed),
        ];
        assert_eq!(toggle_plan(&files, &[0, 1, 2]), (vec![1, 2], true));
    }

    #[test]
    fn fully_viewed_directory_unmarks_everything() {
        let files = vec![
            file("d/a", ViewedState::Viewed),
            file("d/b", ViewedState::Viewed),
        ];
        assert_eq!(toggle_plan(&files, &[0, 1]), (vec![0, 1], false));
    }

    #[test]
    fn dismissed_file_gets_re_marked_not_unmarked() {
        let files = vec![file("d/a", ViewedState::Dismissed)];
        assert_eq!(toggle_plan(&files, &[0]), (vec![0], true));
    }

    fn app(files: Vec<PrFile>, hide_viewed: bool) -> FilesView {
        FilesView {
            dirs: build_tree(&files, &[]).0,
            pr_roots: vec![ROOT],
            scope: None,
            ledger: Ledger::new(&files),
            files,
            collapsed: HashSet::new(),
            hide_viewed,
            table: TableState::default(),
            status: None,
            confirm: None,
            worker: None,
        }
    }

    fn row_labels(a: &FilesView) -> Vec<String> {
        a.rows()
            .into_iter()
            .map(|r| match r {
                RowRef::Dir(d) => a.dirs[d].label.clone(),
                RowRef::File { file, .. } => a.files[file].file_name().to_string(),
            })
            .collect()
    }

    #[test]
    fn rows_walk_the_tree_directories_before_files() {
        let a = app(files(&["a/b/x.ts", "a/z.ts", "root.md"]), false);
        assert_eq!(row_labels(&a), ["a/", "b/", "x.ts", "z.ts", "root.md"]);
    }

    #[test]
    fn hide_viewed_prunes_nested_directories_with_nothing_left() {
        let a = app(
            vec![
                file("a/done/x.ts", ViewedState::Viewed),
                file("a/todo/y.ts", ViewedState::Dismissed),
                file("a/z.ts", ViewedState::Viewed),
            ],
            true,
        );
        assert_eq!(row_labels(&a), ["a/", "todo/", "y.ts"]);
    }

    #[test]
    fn guides_follow_the_tree_and_match_row_order() {
        let a = app(files(&["a/b/x.ts", "a/b/y.ts", "a/z.ts", "root.md"]), false);
        let guided = a.rows_with_guides();
        let order: Vec<RowRef> = guided.iter().map(|(r, _)| *r).collect();
        assert_eq!(order, a.rows());
        let guides: Vec<&str> = guided.iter().map(|(_, g)| g.as_str()).collect();
        // a/ ─┬ b/ ─┬ x.ts
        //     │     └ y.ts
        //     └ z.ts
        // root.md
        assert_eq!(guides, ["", "├─ ", "│  ├─ ", "│  └─ ", "└─ ", ""]);
    }

    #[test]
    fn collapsing_hides_every_descendant_but_keeps_the_row() {
        let mut a = app(files(&["a/b/x.ts", "a/z.ts", "root.md"]), false);
        let a_dir = a.dirs[ROOT].subdirs[0];
        a.collapsed.insert(a_dir);
        assert_eq!(row_labels(&a), ["a/", "root.md"]);
    }

    #[test]
    fn left_folds_an_open_dir_then_steps_out_to_its_parent() {
        let mut a = app(files(&["a/b/x.ts", "a/z.ts"]), false);
        let a_dir = a.dirs[ROOT].subdirs[0];
        let b_dir = a.dirs[a_dir].subdirs[0];
        a.table.select(Some(1)); // b/
        a.go_left(RowRef::Dir(b_dir));
        assert!(a.collapsed.contains(&b_dir));
        a.go_left(RowRef::Dir(b_dir));
        assert_eq!(a.current(), Some(RowRef::Dir(a_dir)));
    }

    fn multi_pr_app() -> FilesView {
        let mut fs = files(&["src/app.ts", "terraform/main.tf", "README.md"]);
        fs[1].pr = 1;
        fs[2].pr = 1;
        let labels = ["app#1".to_string(), "infra#2".to_string()];
        let (dirs, pr_roots) = build_tree(&fs, &labels);
        FilesView {
            dirs,
            pr_roots,
            scope: None,
            ledger: Ledger::new(&fs),
            files: fs,
            collapsed: HashSet::new(),
            hide_viewed: false,
            table: TableState::default(),
            status: None,
            confirm: None,
            worker: None,
        }
    }

    #[test]
    fn several_prs_sit_side_by_side_under_their_own_roots() {
        let a = multi_pr_app();
        assert_eq!(
            row_labels(&a),
            [
                "app#1",
                "src/",
                "app.ts",
                "infra#2",
                "terraform/",
                "main.tf",
                "README.md"
            ]
        );
        assert_eq!(a.viewed_counts(), (0, 3));
    }

    #[test]
    fn scoping_to_one_pr_shows_just_its_tree_and_counts() {
        let mut a = multi_pr_app();
        a.set_scope(Some(1));
        assert_eq!(row_labels(&a), ["terraform/", "main.tf", "README.md"]);
        assert_eq!(a.viewed_counts(), (0, 2));
    }

    #[test]
    fn v_on_a_pr_root_covers_that_pr_only() {
        let a = multi_pr_app();
        let infra = a.pr_roots[1];
        let mut covered = a.dirs[infra].all_files.clone();
        covered.sort();
        assert_eq!(covered, [1, 2]);
    }

    #[test]
    fn m_asks_first_and_only_y_marks_the_generated_files() {
        let mut fs = files(&["src/a.rs", "pnpm-lock.yaml", "ui/__snapshots__/b.snap"]);
        fs[1].generated = Some("lockfile");
        fs[2].generated = Some("snapshot");
        let mut a = app(fs, false);
        a.handle_key(KeyCode::Char('m'));
        // In tree order: the snapshot's directory comes before root files.
        assert_eq!(a.confirm.as_deref(), Some(&[2, 1][..]));
        a.handle_key(KeyCode::Char('n'));
        assert!(a.confirm.is_none());
        assert_eq!(
            a.files[1].viewed,
            ViewedState::Unviewed,
            "cancel writes nothing"
        );
        a.handle_key(KeyCode::Char('m'));
        a.handle_key(KeyCode::Char('y'));
        assert_eq!(a.files[1].viewed, ViewedState::Viewed);
        assert_eq!(a.files[2].viewed, ViewedState::Viewed);
        assert_eq!(a.files[0].viewed, ViewedState::Unviewed);
    }

    #[test]
    fn while_the_dialog_is_open_other_keys_do_nothing() {
        let mut fs = files(&["src/a.rs", "yarn.lock"]);
        fs[1].generated = Some("lockfile");
        let mut a = app(fs, false);
        a.handle_key(KeyCode::Char('m'));
        a.handle_key(KeyCode::Char('v'));
        a.handle_key(KeyCode::Char('H'));
        assert!(a.is_modal() && !a.hide_viewed);
        assert_eq!(a.files[0].viewed, ViewedState::Unviewed);
    }
}
